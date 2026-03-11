/// Fragmentation benchmark: phased server simulation.
///
/// Phase 1 - Ramp up: allocate "connections" with varied buffer sizes.
/// Phase 2 - Churn: close half, reopen with a different size distribution.
/// Phase 3 - Drain: close remaining connections gradually.
///
/// At each phase boundary, snapshot allocator metrics and RSS, then print
/// a fragmentation table comparing active/allocated and mapped/allocated.
///
/// Usage: cargo bench --bench fragmentation --features clmalloc,metrics
///        cargo bench --bench fragmentation --features jemalloc
mod alloc_setup;
mod rss;

use rand::{RngExt, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use std::time::Instant;

struct Connection {
    _buffers: Vec<Vec<u8>>,
}

fn alloc_connection(rng: &mut Xoshiro256PlusPlus, min: usize, max: usize) -> Connection {
    let n = rng.random_range(3..=8u32);
    let buffers = (0..n)
        .map(|_| {
            let size = rng.random_range(min..=max);
            vec![0xABu8; size]
        })
        .collect();
    Connection { _buffers: buffers }
}

struct Snapshot {
    phase: &'static str,
    elapsed_s: f64,
    allocated: u64,
    active: u64,
    mapped: u64,
    rss: u64,
}

fn take_snapshot(phase: &'static str, start: Instant) -> Snapshot {
    let elapsed_s = start.elapsed().as_secs_f64();
    let rss = rss::get_rss();

    #[cfg(feature = "clmalloc")]
    {
        let snap = alloc_setup::ALLOC.snapshot();
        return Snapshot {
            phase,
            elapsed_s,
            allocated: snap.allocated,
            active: snap.active,
            mapped: snap.mapped,
            rss,
        };
    }

    #[cfg(feature = "jemalloc")]
    {
        tikv_jemalloc_ctl::epoch::advance().ok();
        return Snapshot {
            phase,
            elapsed_s,
            allocated: tikv_jemalloc_ctl::stats::allocated::read().unwrap_or(0) as u64,
            active: tikv_jemalloc_ctl::stats::active::read().unwrap_or(0) as u64,
            mapped: tikv_jemalloc_ctl::stats::mapped::read().unwrap_or(0) as u64,
            rss,
        };
    }

    #[cfg(not(any(feature = "clmalloc", feature = "jemalloc")))]
    Snapshot {
        phase,
        elapsed_s,
        allocated: 0,
        active: 0,
        mapped: 0,
        rss,
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn ratio(a: u64, b: u64) -> String {
    if b == 0 {
        "-".to_string()
    } else {
        format!("{:.2}", a as f64 / b as f64)
    }
}

fn print_header(name: &str) {
    println!("\nfragmentation benchmark ({name})");
    println!(
        "  {:<10} {:>6}  {:>12} {:>12} {:>12} {:>12}  {:>12} {:>12}",
        "phase", "time", "allocated", "active", "mapped", "rss", "active/alloc", "mapped/alloc"
    );
}

fn print_snapshot(s: &Snapshot) {
    println!(
        "  {:<10} {:>5.1}s  {:>9.1} MiB {:>9.1} MiB {:>9.1} MiB {:>9.1} MiB  {:>12} {:>12}",
        s.phase,
        s.elapsed_s,
        mib(s.allocated),
        mib(s.active),
        mib(s.mapped),
        mib(s.rss),
        ratio(s.active, s.allocated),
        ratio(s.mapped, s.allocated),
    );
}

fn main() {
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(0xDEAD_BEEF);
    let name = alloc_setup::allocator_name();

    const NUM_CONNECTIONS: usize = 4000;

    // -- Phase 1: Ramp up --
    let start = Instant::now();
    let mut connections: Vec<Option<Connection>> = Vec::with_capacity(NUM_CONNECTIONS);
    for _ in 0..NUM_CONNECTIONS {
        connections.push(Some(alloc_connection(&mut rng, 64, 4096)));
    }

    print_header(name);
    print_snapshot(&take_snapshot("ramp-up", start));

    // -- Phase 2: Churn --
    // Close ~50% of connections (random selection).
    let mut closed = 0;
    for conn in &mut connections {
        if rng.random_bool(0.5) {
            *conn = None;
            closed += 1;
        }
    }

    print_snapshot(&take_snapshot("close-50%", start));

    // Reopen with a different size distribution (larger buffers → "swiss cheese").
    let mut reopened = 0;
    for conn in &mut connections {
        if conn.is_none() {
            *conn = Some(alloc_connection(&mut rng, 1024, 8192));
            reopened += 1;
        }
    }

    print_snapshot(&take_snapshot("reopen", start));

    // Second churn round: close another 50%.
    let mut closed2 = 0;
    for conn in &mut connections {
        if conn.is_some() && rng.random_bool(0.5) {
            *conn = None;
            closed2 += 1;
        }
    }

    // Reopen with yet another distribution.
    for conn in &mut connections {
        if conn.is_none() {
            *conn = Some(alloc_connection(&mut rng, 128, 2048));
        }
    }

    print_snapshot(&take_snapshot("churn-2", start));

    // -- Phase 3: Drain --
    // Close all connections gradually (25% at a time).
    for pct in [25, 50, 75, 100] {
        let target = NUM_CONNECTIONS * pct / 100;
        let mut current_closed = connections.iter().filter(|c| c.is_none()).count();
        for conn in &mut connections {
            if current_closed >= target {
                break;
            }
            if conn.is_some() {
                *conn = None;
                current_closed += 1;
            }
        }
        let label: &'static str = match pct {
            25 => "drain-25%",
            50 => "drain-50%",
            75 => "drain-75%",
            _ => "drain-100%",
        };
        print_snapshot(&take_snapshot(label, start));
    }

    // Summarize.
    let _ = (closed, reopened, closed2);
    println!();
}
