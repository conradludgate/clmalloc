/// Tokio work-stealing allocator benchmark.
///
/// Simulates an async server: a producer spawns request-handler tasks at a
/// controlled rate, each handler allocates buffers, does work across coop
/// yield points, and drops. Concurrency is bounded by a semaphore to avoid
/// flooding the global queue — keeping tasks in per-worker local queues
/// where work-stealing actually occurs.
///
/// Usage: cargo bench --bench tokio_worksteal -- [duration_secs concurrency buf_min buf_max work_iterations]
/// Default: 5s, 4*CPUs concurrency, 256-4096 byte buffers, 200 work iterations
mod alloc_setup;

use rand::{RngExt, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let (duration_secs, concurrency, buf_min, buf_max, work_iters) = if args.len() > 5 {
        (
            args[1].parse::<u64>().unwrap(),
            args[2].parse::<usize>().unwrap(),
            args[3].parse::<usize>().unwrap(),
            args[4].parse::<usize>().unwrap(),
            args[5].parse::<usize>().unwrap(),
        )
    } else {
        (5, 24 * cpus, 256, 4096, 200)
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cpus)
        .enable_time()
        .build()
        .unwrap();

    let bytes_allocated = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(concurrency));

    let start = Instant::now();

    let total = rt.block_on(async {
        let bytes_allocated = bytes_allocated.clone();
        let semaphore = semaphore.clone();

        tokio::spawn(async move {
            let mut seed: u64 = 42;
            let mut spawned: u64 = 0;
            let deadline = Duration::from_secs(duration_secs);

            loop {
                let permit = semaphore.clone().acquire_owned().await.unwrap();

                if start.elapsed() >= deadline {
                    break;
                }

                seed = seed.wrapping_add(1);
                spawned += 1;

                tokio::spawn(handle_request(
                    seed,
                    bytes_allocated.clone(),
                    permit,
                    buf_min,
                    buf_max,
                    work_iters,
                ));
            }

            // Wait for in-flight tasks to drain
            let _ = semaphore.clone().acquire_many(concurrency as u32).await;

            spawned
        })
        .await
        .unwrap()
    });

    let elapsed = start.elapsed().as_secs_f64();
    let total_bytes = bytes_allocated.load(Ordering::Relaxed);

    println!(
        "tokio work-stealing benchmark ({})",
        alloc_setup::allocator_name()
    );
    println!("  workers:      {cpus}");
    println!("  concurrency:  {concurrency} tasks");
    println!("  buf range:    {buf_min}-{buf_max} bytes");
    println!("  work/task:    {work_iters} iterations");
    println!("  duration:     {elapsed:.3}s");
    println!("  completed:    {total} tasks");
    println!("  throughput:   {:.0} tasks/sec", total as f64 / elapsed);
    println!(
        "  alloc rate:   {:.2} GB/sec",
        total_bytes as f64 / elapsed / 1_000_000_000.0
    );
}

/// Simulates a single request handler: allocate buffers, do work across coop
/// yield points (where the scheduler can steal us to another worker), then drop.
/// The permit is released on exit, allowing the spawner to feed a replacement.
async fn handle_request(
    seed: u64,
    bytes_allocated: Arc<AtomicU64>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    buf_min: usize,
    buf_max: usize,
    work_iters: usize,
) {
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let buf_len = rng
        .random_range(buf_min..buf_max.max(buf_min + 1))
        .div_ceil(8);

    // Allocate request buffer (align 8 via u64, no zeroing)
    let mut buf = Box::<[u64]>::new_uninit_slice(buf_len);
    bytes_allocated.fetch_add((buf_len * size_of::<u64>()) as u64, Ordering::Relaxed);

    // Phase 1: simulate reading/parsing the request
    for i in 0..work_iters {
        let idx = i % buf_len;
        buf[idx].write(i as u64);
        black_box(&buf[idx]);
        tokio::task::consume_budget().await;
    }

    // Phase 2: simulate processing (may now be on a different worker)
    for i in 0..work_iters {
        let idx = (buf_len - 1) - (i % buf_len);
        let val = unsafe { buf[idx].assume_init() };
        buf[idx].write(val.wrapping_add(1));
        black_box(&buf[idx]);
        tokio::task::consume_budget().await;
    }

    // Phase 3: allocate response buffer, copy some data
    if rng.random_ratio(2, 3) {
        let resp_len = rng
            .random_range(buf_min..buf_max.max(buf_min + 1))
            .div_ceil(8);
        let mut resp = Box::<[u64]>::new_uninit_slice(resp_len);
        bytes_allocated.fetch_add((resp_len * size_of::<u64>()) as u64, Ordering::Relaxed);

        resp[0].write(unsafe { buf[0].assume_init() });
        if resp_len > 1 {
            resp[resp_len - 1].write(unsafe { buf[buf_len - 1].assume_init() });
        }
        black_box(&mut resp);
    }

    // buf drops here — may be freed on a different worker than where allocated
    black_box(&mut buf);
    // _permit drops here, releasing the semaphore slot
}
