/// Port of Paul Larson's server workload benchmark (larsonN).
///
/// Simulates a server where connections are handled by threads. Each thread
/// owns a slice of allocation slots and repeatedly frees/reallocates them.
/// When a thread finishes its rounds, it spawns a NEW OS thread that inherits
/// the same slots — so the successor frees memory allocated by its predecessor.
/// This exercises cross-thread deallocation paths in the allocator.
///
/// Usage: cargo bench --bench larson -- [duration_secs min_size max_size chunks_per_thread num_rounds seed num_threads]
/// Default: 5s, 256-4096 bytes, 1000 chunks/thread, 100 rounds, seed 4141, threads=CPUs
mod alloc_setup;

use rand::{RngExt, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// -- Benchmark core ----------------------------------------------------------

fn random_block(
    rng: &mut Xoshiro256PlusPlus,
    min_size: usize,
    max_size: usize,
) -> Box<[MaybeUninit<u64>]> {
    let byte_size = rng.random_range(min_size..=max_size);
    let len = byte_size.div_ceil(8);
    Box::new_uninit_slice(len)
}

/// Touch the allocation as bytes, matching original larsonN behavior:
/// write byte 0, read it back, write byte 1.
fn touch(block: &mut Box<[MaybeUninit<u64>]>) {
    let ptr = block.as_mut_ptr().cast::<u8>();
    let byte_len = block.len() * size_of::<u64>();
    unsafe {
        ptr.write_volatile(b'a');
        let _ = ptr.read_volatile();
        if byte_len > 1 {
            ptr.add(1).write_volatile(b'b');
        }
    }
}

struct ThreadState {
    blocks: Vec<Box<[MaybeUninit<u64>]>>,
    num_rounds: usize,
    min_size: usize,
    max_size: usize,
    rng: Xoshiro256PlusPlus,
    allocs: u64,
    frees: u64,
    generations: u64,
}

struct ThreadResult {
    allocs: u64,
    frees: u64,
    generations: u64,
}

static STOP: AtomicBool = AtomicBool::new(false);

fn exercise_heap(state: &mut ThreadState) {
    let asize = state.blocks.len();
    let total_ops = state.num_rounds * asize;

    state.generations += 1;

    for _ in 0..total_ops {
        let victim = state.rng.random_range(0..asize);

        // Drop existing block, allocate replacement
        let mut block = random_block(&mut state.rng, state.min_size, state.max_size);
        touch(&mut block);

        // Swap in the new block — the old one is dropped (freed) here
        std::mem::swap(&mut state.blocks[victim], &mut block);
        drop(block);

        state.frees += 1;
        state.allocs += 1;

        if STOP.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// Each generation is a distinct OS thread. When a generation finishes its
/// rounds without STOP being set, it spawns a successor thread that inherits
/// the allocation slots — the successor will free memory allocated by this thread.
fn thread_chain(mut state: ThreadState, result_tx: mpsc::Sender<ThreadResult>) {
    exercise_heap(&mut state);

    if STOP.load(Ordering::Relaxed) {
        // Blocks are dropped automatically when state goes out of scope
        let _ = result_tx.send(ThreadResult {
            allocs: state.allocs,
            frees: state.frees,
            generations: state.generations,
        });
    } else {
        thread::spawn(move || thread_chain(state, result_tx));
    }
}

fn warmup(
    blocks: &mut [Box<[MaybeUninit<u64>]>],
    min_size: usize,
    max_size: usize,
    rng: &mut Xoshiro256PlusPlus,
) {
    let num_chunks = blocks.len();

    // Fisher-Yates shuffle
    for i in (1..num_chunks).rev() {
        let j = rng.random_range(0..i);
        blocks.swap(i, j);
    }

    // 4x replace rounds to warm up the allocator
    for _ in 0..4 * num_chunks {
        let victim = rng.random_range(0..num_chunks);
        let mut block = random_block(rng, min_size, max_size);
        touch(&mut block);
        blocks[victim] = block;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let (duration_secs, min_size, max_size, chunks_per_thread, num_rounds, seed, num_threads) =
        if args.len() > 7 {
            (
                args[1].parse::<u64>().unwrap(),
                args[2].parse::<usize>().unwrap(),
                args[3].parse::<usize>().unwrap(),
                args[4].parse::<usize>().unwrap(),
                args[5].parse::<usize>().unwrap(),
                args[6].parse::<u64>().unwrap(),
                args[7].parse::<usize>().unwrap(),
            )
        } else {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            (5, 256, 4096, 1000, 100, 4141u64, cpus)
        };

    assert!(min_size >= 1, "min_size must be >= 1");
    assert!(max_size >= min_size, "max_size must be >= min_size");

    STOP.store(false, Ordering::SeqCst);

    let total_chunks = num_threads * chunks_per_thread;
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);

    let mut all_blocks: Vec<Box<[MaybeUninit<u64>]>> = (0..total_chunks)
        .map(|_| random_block(&mut rng, min_size, max_size))
        .collect();

    warmup(&mut all_blocks, min_size, max_size, &mut rng);

    let mut receivers = Vec::with_capacity(num_threads);
    let mut drain = all_blocks.into_iter();

    for _ in 0..num_threads {
        let blocks: Vec<Box<[MaybeUninit<u64>]>> = (&mut drain).take(chunks_per_thread).collect();
        let (tx, rx) = mpsc::channel();
        receivers.push(rx);

        let state = ThreadState {
            blocks,
            num_rounds,
            min_size,
            max_size,
            rng: Xoshiro256PlusPlus::seed_from_u64(rng.random()),
            allocs: 0,
            frees: 0,
            generations: 0,
        };

        thread::spawn(move || thread_chain(state, tx));
    }

    let start = Instant::now();
    thread::sleep(Duration::from_secs(duration_secs));
    STOP.store(true, Ordering::SeqCst);

    let mut total_allocs: u64 = 0;
    let mut total_frees: u64 = 0;
    let mut total_generations: u64 = 0;

    for rx in receivers {
        match rx.recv() {
            Ok(result) => {
                total_allocs += result.allocs;
                total_frees += result.frees;
                total_generations += result.generations;
            }
            Err(_) => eprintln!("warning: thread chain dropped without reporting"),
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let total_ops = total_allocs + total_frees;

    println!("larson benchmark ({})", alloc_setup::allocator_name());
    println!("  threads:      {num_threads}");
    println!("  size range:   {min_size}-{max_size} bytes");
    println!("  chunks/thr:   {chunks_per_thread}");
    println!("  rounds/gen:   {num_rounds}");
    println!("  duration:     {elapsed:.3}s");
    println!("  allocs:       {total_allocs}");
    println!("  frees:        {total_frees}");
    println!("  generations:  {total_generations}");
    println!("  throughput:   {:.0} ops/sec", total_ops as f64 / elapsed);
}
