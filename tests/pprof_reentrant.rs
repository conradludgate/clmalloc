//! Integration test: pprof reentrancy guard with clmalloc as `#[global_allocator]`.
//!
//! With sample_interval=1, every allocation triggers sampling. Without the
//! reentrancy guard, the internal allocations inside `record_sample` (Vec,
//! HashMap, backtrace) would re-enter the profiler and stack overflow.

#![cfg(feature = "pprof")]

use clmalloc::ClMalloc;
use clmalloc::pprof::PprofConfig;

#[global_allocator]
static ALLOC: ClMalloc = ClMalloc::new();

// r[verify pprof.no-reentrant-sample]
#[test]
fn pprof_100pct_sample_rate_does_not_stack_overflow() {
    ALLOC.set_pprof_config(Some(PprofConfig { sample_interval: 1 }));

    for _ in 0..100 {
        let v: Vec<u8> = vec![0xAB; 256];
        drop(v);
    }

    let mut buf = Vec::new();
    ALLOC.dump_heap_profile(&mut buf).unwrap();
    assert!(!buf.is_empty());

    ALLOC.set_pprof_config(None);
}
