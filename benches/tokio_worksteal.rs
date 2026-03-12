/// Tokio work-stealing allocator benchmark.
///
/// Simulates an async server: a producer spawns request-handler tasks at a
/// controlled rate, each handler allocates buffers, does work across coop
/// yield points, and drops. Concurrency is bounded by a semaphore to avoid
/// flooding the global queue — keeping tasks in per-worker local queues
/// where work-stealing actually occurs.
///
/// Usage: cargo bench --bench tokio_worksteal -- [duration_secs concurrency buf_min buf_max work_iterations runs]
/// Default: 5s, 24*CPUs concurrency, 256-4096 byte buffers, 50 alloc iterations/phase, 3 runs
mod alloc_setup;
mod bench_stats;

use bench_stats::RunStats;
use rand::{RngExt, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

struct TrialResult {
    tasks_per_sec: f64,
    total_tasks: u64,
    total_bytes: u64,
    elapsed: f64,
}

fn run_trial(
    cpus: usize,
    duration_secs: u64,
    concurrency: usize,
    buf_min: usize,
    buf_max: usize,
    work_iters: usize,
) -> TrialResult {
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

            let _ = semaphore.clone().acquire_many(concurrency as u32).await;

            spawned
        })
        .await
        .unwrap()
    });

    let elapsed = start.elapsed().as_secs_f64();
    let total_bytes = bytes_allocated.load(Ordering::Relaxed);

    TrialResult {
        tasks_per_sec: total as f64 / elapsed,
        total_tasks: total,
        total_bytes,
        elapsed,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let (duration_secs, concurrency, buf_min, buf_max, work_iters, runs) = if args.len() > 6 {
        (
            args[1].parse::<u64>().unwrap(),
            args[2].parse::<usize>().unwrap(),
            args[3].parse::<usize>().unwrap(),
            args[4].parse::<usize>().unwrap(),
            args[5].parse::<usize>().unwrap(),
            args[6].parse::<usize>().unwrap(),
        )
    } else {
        (5, 24 * cpus, 256, 4096, 50, 3)
    };

    assert!(runs >= 1, "runs must be >= 1");

    println!(
        "tokio work-stealing benchmark ({})",
        alloc_setup::allocator_name()
    );
    println!("  workers:      {cpus}");
    println!("  concurrency:  {concurrency} tasks");
    println!("  buf range:    {buf_min}-{buf_max} bytes");
    println!("  work/task:    {work_iters} iterations");
    println!("  runs:         {runs}");

    let mut stats = RunStats::new();

    for run in 1..=runs {
        let result = run_trial(
            cpus,
            duration_secs,
            concurrency,
            buf_min,
            buf_max,
            work_iters,
        );

        let tasks_k = result.tasks_per_sec / 1000.0;
        let gb_per_sec = result.total_bytes as f64 / result.elapsed / 1_000_000_000.0;
        println!(
            "  run {run}/{runs}:    {tasks_k:.0}k tasks/sec  ({:.3}s, {} tasks, {gb_per_sec:.2} GB/s)",
            result.elapsed, result.total_tasks,
        );
        stats.push(result.tasks_per_sec / 1000.0);
    }

    stats.print("throughput", "k tasks/sec");
}

/// Simulates a request handler with heavy allocation pressure across yield
/// points. Each phase allocates intermediate buffers of varying sizes, works
/// with them briefly, then drops — mimicking real server handlers that build
/// up and tear down data structures (parsed requests, DB rows, serialized
/// responses) throughout their lifetime. Cross-worker deallocation happens
/// naturally because yield points let the scheduler steal the task.
async fn handle_request(
    seed: u64,
    bytes_allocated: Arc<AtomicU64>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    buf_min: usize,
    buf_max: usize,
    work_iters: usize,
) {
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut total_bytes: u64 = 0;

    // Phase 1: parse request — allocate header + body buffers, yield, drop
    for _ in 0..work_iters {
        let len = rng
            .random_range(buf_min..buf_max.max(buf_min + 1))
            .div_ceil(8);
        let mut buf = Box::<[u64]>::new_uninit_slice(len);
        total_bytes += (len * size_of::<u64>()) as u64;
        buf[0].write(len as u64);
        black_box(&mut buf);
        tokio::task::consume_budget().await;
    }

    // Phase 2: process — allocate working sets of mixed sizes, yield between
    for _ in 0..work_iters {
        let small_len = rng.random_range(1..=8);
        let mut small = Box::<[u64]>::new_uninit_slice(small_len);
        total_bytes += (small_len * size_of::<u64>()) as u64;
        small[0].write(42);
        black_box(&mut small);

        let large_len = rng
            .random_range(buf_min..buf_max.max(buf_min + 1))
            .div_ceil(8);
        let mut large = Box::<[u64]>::new_uninit_slice(large_len);
        total_bytes += (large_len * size_of::<u64>()) as u64;
        large[0].write(99);
        black_box(&mut large);

        tokio::task::consume_budget().await;
    }

    // Phase 3: build response — accumulate buffers, then drop all at once
    {
        let mut parts: Vec<Box<[u64]>> = Vec::with_capacity(work_iters);
        for _ in 0..work_iters {
            let len = rng
                .random_range(buf_min / 2..buf_max.max(buf_min + 1))
                .div_ceil(8);
            let mut buf = Box::<[u64]>::new_uninit_slice(len);
            total_bytes += (len * size_of::<u64>()) as u64;
            buf[0].write(0xDEAD);
            parts.push(unsafe { buf.assume_init() });

            if parts.len().is_multiple_of(4) {
                tokio::task::consume_budget().await;
            }
        }
        black_box(&mut parts);
    }

    bytes_allocated.fetch_add(total_bytes, Ordering::Relaxed);
}
