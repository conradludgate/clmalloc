# Benchmarks

## Available benchmarks

| Benchmark | What it measures |
|-----------|-----------------|
| `larson` | Server workload with cross-thread deallocation. Threads own allocation slots and repeatedly free/realloc. Successor threads inherit slots from predecessors, exercising remote free paths. |
| `cache_scratch` | False sharing detection (Hoard benchmark). Main thread allocates objects distributed to workers; workers free and realloc, writing every byte. Poor allocators place objects on shared cache lines. |
| `tokio_worksteal` | Async server throughput with heavy allocation pressure. Handler tasks allocate/free intermediate buffers of mixed sizes across yield points, exercising cross-worker deallocation via work-stealing. |
| `fragmentation` | Memory efficiency under churn. Three phases: ramp up, churn (close half, reopen with different sizes), drain. Reports fragmentation ratios at each phase boundary. Requires `metrics` feature. |

## Running

Select the allocator with a feature flag:

```sh
cargo bench --bench larson --features clmalloc
cargo bench --bench larson --features jemalloc
cargo bench --bench larson --features mimalloc
cargo bench --bench larson --features snmalloc
cargo bench --bench larson                        # system allocator
```

For fragmentation, also enable `metrics`:

```sh
cargo bench --bench fragmentation --features clmalloc,metrics
cargo bench --bench fragmentation --features jemalloc
```

Each benchmark accepts CLI arguments for tuning. Run with `--` to see
defaults printed at the start of each run. For example:

```sh
# larson: duration_secs min_size max_size chunks_per_thread num_rounds seed num_threads runs
cargo bench --bench larson --features clmalloc -- 10 256 4096 1000 100 4141 8 5

# cache_scratch: num_threads iterations object_size num_rounds
cargo bench --bench cache_scratch --features clmalloc -- 8 1000 64 1000

# tokio_worksteal: duration_secs concurrency buf_min buf_max alloc_iterations runs
cargo bench --bench tokio_worksteal --features clmalloc -- 10 768 256 4096 50 5
```

Benchmarks with `runs` support (larson, tokio_worksteal) default to 3 runs
and report median, mean ± stddev, min, max. The median is the primary metric
reported in the tables below.

## Results

### Throughput — Apple M4 Max (aarch64, 16 threads)

Higher is better.

| Benchmark | clmalloc | jemalloc | mimalloc | system |
|-----------|----------|----------|----------|--------|
| larson (M ops/s) | 751 | **1,267** | 1,045 | 90 |
| cache_scratch 1T (ops/s) | 57,352 | **58,057** | 54,229 | 54,527 |
| cache_scratch 8T (ops/s) | **2,937,171** | 2,417,314 | 2,487,949 | 2,148,733 |
| tokio_worksteal (k tasks/s) | **1,357** | 1,333 | 1,264 | 433 |

### Fragmentation — Apple M4 Max (aarch64)

active/allocated ratio (lower is better). Measures how much memory the
allocator keeps active relative to what the application requested.

| Phase | clmalloc | jemalloc |
|-------|----------|----------|
| ramp-up | 1.07 | **1.01** |
| close-50% | 2.13 | **1.88** |
| reopen | 1.28 | **1.14** |
| churn-2 | 2.00 | **1.70** |
| drain-25% | 2.73 | **1.86** |
| drain-50% | 3.66 | **2.16** |
| drain-75% | 5.37 | **2.38** |
| drain-100% | 365.52 | **4.10** |

Note: clmalloc uses deferred purge with a dirty threshold and a lock-free
slab cache (up to 512 slabs). The fragmentation benchmark is single-threaded
and exercises one size class, so the slab cache retains dirty pages longer
than in multi-threaded workloads where pages get reused across classes.

### Throughput — Intel Xeon 8375C (x86_64, 32 threads)

Higher is better.

| Benchmark | clmalloc | jemalloc | mimalloc | snmalloc | system |
|-----------|----------|----------|----------|----------|--------|
| larson (M ops/s) | 981 | 348 | 817 | **1,020** | 374 |
| cache_scratch 1T (ops/s) | **55,482** | 54,942 | 54,037 | 55,015 | 54,940 |
| cache_scratch 8T (ops/s) | **3,140,760** | 2,580,206 | 1,730,797 | 2,947,750 | 611,753 |
| tokio_worksteal (k tasks/s) | 1,424 | 1,402 | 1,153 | **1,454** | 553 |

### Fragmentation — Intel Xeon 8375C (x86_64)

active/allocated ratio (lower is better). Measures how much memory the
allocator keeps active relative to what the application requested.

| Phase | clmalloc | jemalloc |
|-------|----------|----------|
| ramp-up | 1.07 | **1.00** |
| close-50% | 2.14 | **1.67** |
| reopen | 1.28 | **1.10** |
| churn-2 | 2.00 | **1.40** |
| drain-25% | 2.16 | **1.50** |
| drain-50% | 2.37 | **1.67** |
| drain-75% | 2.84 | **1.78** |
| drain-100% | 23.83 | **2.22** |
