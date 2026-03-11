# clmalloc

A thread-caching slab allocator for Rust, inspired by mimalloc. Lock-free
fast path, per-thread heaps, 49 size classes, bump allocation for fresh
slabs. The goal is to become the best Rust allocator for tokio applications
-- this is a work in progress.

## Usage

```rust
use clmalloc::ClMalloc;

#[global_allocator]
static ALLOC: ClMalloc = ClMalloc::new();
```

### Metrics

Enable the `metrics` feature to collect allocation statistics:

```toml
[dependencies]
clmalloc = { path = ".", features = ["metrics"] }
```

```rust
let snap = ALLOC.snapshot();
println!("allocated: {} bytes", snap.allocated);
println!("active:    {} bytes", snap.active);
println!("mapped:    {} bytes", snap.mapped);
```

## How-to

### Collecting metrics

`ALLOC.snapshot()` returns a `MetricsSnapshot` aggregated across all
threads. Key fields:

| Field | Meaning |
|-------|---------|
| `allocated` | Application-requested bytes currently outstanding |
| `active` | Bytes in slab pages with at least one live allocation |
| `mapped` | Total virtual address space obtained from the OS |

Per-size-class counters (`class_alloc_count`, `class_alloc_bytes`, etc.)
are stored as independent per-bucket values, suitable for building
Prometheus histograms using the size class boundaries as `le` labels.

### Heap profiling

Enable the `pprof` feature for sampling-based heap profiling compatible
with the pprof ecosystem (same model as jemalloc's `prof.*`):

```toml
[dependencies]
clmalloc = { path = ".", features = ["pprof"] }
```

```rust
use clmalloc::pprof::PprofConfig;

ALLOC.set_pprof_config(Some(PprofConfig::default()));

// ... run workload ...

let mut file = std::fs::File::create("heap.pb.gz").unwrap();
ALLOC.dump_heap_profile(&mut file).unwrap();
```

Then analyze with:

```sh
go tool pprof heap.pb.gz
```

### Benchmarking

See [benches/README.md](benches/README.md) for available benchmarks,
how to run them, and results.

## Reference

### Features

| Feature | Purpose |
|---------|---------|
| `metrics` | Allocation counters and `MetricsSnapshot` |
| `pprof` | Sampling heap profiler (pprof protobuf output) |
| `clmalloc` | Select clmalloc in benchmarks |
| `jemalloc` | Select jemalloc in benchmarks |
| `mimalloc` | Select mimalloc in benchmarks |
| `snmalloc` | Select snmalloc in benchmarks |

### `ClMalloc` API

| Method | Feature | Description |
|--------|---------|-------------|
| `new() -> Self` | -- | Create allocator (const, use in a static) |
| `snapshot(&self) -> MetricsSnapshot` | `metrics` | Aggregate metrics across all threads |
| `set_pprof_config(&self, Option<PprofConfig>)` | `pprof` | Activate (`Some`) or deactivate (`None`) heap profiling |
| `dump_heap_profile(&self, &mut dyn Write)` | `pprof` | Write gzip'd pprof protobuf |

### `MetricsSnapshot` fields

**Global gauges**

`allocated`, `active`, `mapped`, `alloc_bytes`, `free_bytes`

**Per-size-class arrays** (indexed by class 0..49)

`class_alloc_count`, `class_dealloc_count`, `class_alloc_bytes`,
`class_free_bytes`, `class_live_count`, `class_live_bytes`,
`cache_hit_count`, `cache_flush_count`, `abandon_count`, `adopt_count`

**Pool counters**

`segment_mmap_count`, `segment_munmap_count`, `slab_purge_count`,
`pool_lock_count`

**Large allocation counters**

`large_alloc_count`, `large_alloc_bytes`, `large_dealloc_bytes`,
`remote_free_count`

### Size classes

49 classes from 8 bytes to 32 KiB, plus a dedicated 32 KiB class.
Each power-of-two range is subdivided into 4 sub-steps, keeping
internal fragmentation below 25%. Allocations above 32 KiB bypass
the slab layer and go directly to `mmap`/`munmap`.

All allocations are aligned to at least 8 bytes. If the requested
alignment exceeds the size class, the allocation is rounded up to
the next class whose size is a multiple of the alignment.

## Design

Four-layer architecture:

```
  GlobalAlloc        per-thread dispatch, ZST/large bypass
       |
  Thread Heap        per-class active slab + cache + page queues
       |
  Page Pool          2 MiB segment carving, free list, abandon/adopt
       |
  PageAllocator      mmap/munmap (production) or alloc_zeroed (tests)
```

**Allocation fast path:** cache pop -> local free list pop -> bump
pointer advance. All three are non-atomic single-threaded operations.

**Key design choices:**

- Bump allocation for fresh slabs (O(1) init, no free list pre-build)
- Treiber stack (lock-free CAS) for cross-thread deallocation
- Per-class free cache absorbs non-active-slab frees, flushed in
  sorted batches to minimize CAS operations
- `madvise(MADV_DONTNEED)` purge on returned slabs to reduce RSS
  without unmapping virtual address space

See `specs/` for formal specifications with tracey coverage annotations.
