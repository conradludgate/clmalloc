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
| larson (M ops/s) | 1,133 | **1,448** | 1,176 | 97 |
| cache_scratch 1T (ops/s) | 56,370 | 51,367 | 51,223 | **58,258** |
| cache_scratch 8T (ops/s) | **3,408,244** | 1,766,135 | 3,069,053 | 3,120,176 |
| tokio_worksteal (tasks/s) | 1,157,720 | 1,187,459 | 1,170,425 | **1,217,922** |

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

### x86_64-v3

Coming soon.
