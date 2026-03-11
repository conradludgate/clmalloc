# Benchmarks

## Available benchmarks

| Benchmark | What it measures |
|-----------|-----------------|
| `larson` | Server workload with cross-thread deallocation. Threads own allocation slots and repeatedly free/realloc. Successor threads inherit slots from predecessors, exercising remote free paths. |
| `cache_scratch` | False sharing detection (Hoard benchmark). Main thread allocates objects distributed to workers; workers free and realloc, writing every byte. Poor allocators place objects on shared cache lines. |
| `tokio_worksteal` | Async server throughput. Producer spawns handler tasks at a controlled rate, each allocating buffers across yield points. Bounded concurrency keeps tasks in per-worker local queues. |
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
# larson: duration_secs min_size max_size chunks_per_thread num_rounds seed num_threads
cargo bench --bench larson --features clmalloc -- 10 256 4096 1000 100 4141 8

# cache_scratch: num_threads iterations object_size num_rounds
cargo bench --bench cache_scratch --features clmalloc -- 8 1000 64 1000

# tokio_worksteal: duration_secs concurrency buf_min buf_max work_iterations
cargo bench --bench tokio_worksteal --features clmalloc -- 10 32 256 4096 200
```

## Results

### Throughput — Apple M4 Max (aarch64, 16 threads)

Higher is better.

| Benchmark | clmalloc | jemalloc | mimalloc | system |
|-----------|----------|----------|----------|--------|
| larson (M ops/s) | 1,290 | **1,343** | 1,224 | 90 |
| cache_scratch 1T (ops/s) | **57,604** | 55,935 | 54,799 | 54,303 |
| cache_scratch 8T (ops/s) | **2,489,110** | 2,144,293 | 2,196,821 | 2,434,108 |
| tokio_worksteal (tasks/s) | 1,168,618 | 1,183,229 | 1,172,181 | **1,224,495** |

### Fragmentation — Apple M4 Max (aarch64)

active/allocated ratio (lower is better). Measures how much memory the
allocator keeps active relative to what the application requested.

| Phase | clmalloc | jemalloc |
|-------|----------|----------|
| ramp-up | 1.07 | **1.01** |
| close-50% | 2.13 | **1.88** |
| reopen | 1.44 | **1.14** |
| churn-2 | 2.39 | **1.70** |
| drain-25% | 2.60 | **1.86** |
| drain-50% | 2.70 | **2.16** |
| drain-75% | 2.83 | **2.38** |
| drain-100% | 90.11 | **4.10** |

Note: clmalloc uses deferred purge with a 64-slab dirty threshold. The
fragmentation benchmark exercises single-threaded churn on a single size
class, which does not trigger the high-water purge path — the active
metric reflects dirty slabs held for fast reuse, not a true leak.

### Throughput — Intel Xeon 8375C (x86_64, 32 threads)

Higher is better.

| Benchmark | clmalloc | jemalloc | mimalloc | snmalloc | system |
|-----------|----------|----------|----------|----------|--------|
| larson (M ops/s) | 187 | 340 | 894 | **999** | 232 |
| cache_scratch 1T (ops/s) | **55,054** | 54,942 | 54,037 | 55,015 | 54,940 |
| cache_scratch 8T (ops/s) | 1,652,468 | 2,580,206 | 1,730,797 | **2,947,750** | 611,753 |
| tokio_worksteal (tasks/s) | **1,169,087** | 876,086 | 906,352 | 968,087 | 717,646 |

### Fragmentation — Intel Xeon 8375C (x86_64)

active/allocated ratio (lower is better). Measures how much memory the
allocator keeps active relative to what the application requested.

| Phase | clmalloc | jemalloc |
|-------|----------|----------|
| ramp-up | 1.07 | **1.00** |
| close-50% | 2.14 | **1.67** |
| reopen | 1.44 | **1.10** |
| churn-2 | 2.39 | **1.40** |
| drain-25% | 2.60 | **1.50** |
| drain-50% | 2.70 | **1.67** |
| drain-75% | 2.84 | **1.78** |
| drain-100% | 91.69 | **2.22** |
