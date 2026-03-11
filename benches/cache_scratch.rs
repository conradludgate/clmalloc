/// Port of Hoard's cache-scratch benchmark.
///
/// Tests whether the allocator avoids false sharing between threads.
/// The main thread allocates N objects (likely contiguous in memory),
/// distributes one to each worker. Each worker frees it, then repeatedly
/// allocates a same-sized object, writes every byte, and frees it.
///
/// If the allocator recycles freed objects into a global pool, workers
/// may get memory on the same cache line → false sharing → poor scaling.
/// A good allocator uses per-thread/per-CPU pools and scales linearly.
///
/// Compare single-threaded vs N-threaded:
///   cargo bench --bench cache_scratch -- 1 1000 64 1000
///   cargo bench --bench cache_scratch -- N 1000 64 1000
mod alloc_setup;

use std::mem::MaybeUninit;
use std::thread;
use std::time::Instant;

fn worker(initial: Box<[MaybeUninit<u64>]>, obj_len: usize, iterations: usize, repetitions: usize) {
    drop(initial);

    for _ in 0..iterations {
        let mut obj = Box::<[u64]>::new_uninit_slice(obj_len);
        let ptr = obj.as_mut_ptr().cast::<u8>();
        let byte_len = obj_len * size_of::<u64>();

        for _ in 0..repetitions {
            for k in 0..byte_len {
                unsafe {
                    ptr.add(k).write_volatile(k as u8);
                    let _ = ptr.add(k).read_volatile();
                }
            }
        }

        drop(obj);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let (nthreads, iterations, obj_size, repetitions) = if args.len() > 4 {
        (
            args[1].parse::<usize>().unwrap(),
            args[2].parse::<usize>().unwrap(),
            args[3].parse::<usize>().unwrap(),
            args[4].parse::<usize>().unwrap(),
        )
    } else {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        (cpus, 1000, 64, 1000)
    };

    assert!(obj_size >= 1, "obj_size must be >= 1");

    let obj_len = obj_size.div_ceil(8);

    // Allocate objects on the main thread (likely contiguous in memory)
    let initial_objs: Vec<Box<[MaybeUninit<u64>]>> = (0..nthreads)
        .map(|_| Box::new_uninit_slice(obj_len))
        .collect();

    let reps_per_thread = repetitions / nthreads;

    let start = Instant::now();

    let handles: Vec<_> = initial_objs
        .into_iter()
        .map(|obj| {
            thread::spawn(move || {
                worker(obj, obj_len, iterations, reps_per_thread);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let elapsed = start.elapsed();

    println!(
        "cache-scratch benchmark ({})",
        alloc_setup::allocator_name()
    );
    println!("  threads:      {nthreads}");
    println!("  obj_size:     {obj_size} bytes");
    println!("  iterations:   {iterations}");
    println!("  repetitions:  {repetitions} (total), {reps_per_thread} (per thread)");
    println!("  elapsed:      {:.3}s", elapsed.as_secs_f64());
    println!(
        "  ops/sec:      {:.0}",
        (nthreads as f64 * iterations as f64) / elapsed.as_secs_f64()
    );
}
