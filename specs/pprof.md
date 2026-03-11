# Heap Profiling (pprof)

Sampling-based heap profiling compatible with the pprof ecosystem.
Mirrors jemalloc's `prof.*` facility: Poisson per-byte sampling with
unbiased estimation, stack trace capture, and protobuf dump output.

Design reference: jemalloc PROFILING_INTERNALS.md (Vitter 1987 geometric
sampling, per-byte Bernoulli strategy).

## Activation

r[pprof.feature-gate]
Heap profiling MUST be gated behind a compile-time feature flag (`pprof`).
When the feature is disabled, all profiling code MUST be compiled out with
zero overhead on the allocation fast path.

r[pprof.activate]
Profiling MUST be activatable and deactivatable at runtime via a single
configuration call: `set_pprof_config(Option<PprofConfig>)`. Passing
`Some(config)` activates profiling with the given settings; passing
`None` deactivates it. The default state MUST be inactive.

r[pprof.sample-interval]
`PprofConfig` MUST include a `sample_interval` field specifying the
mean bytes between samples. The default SHOULD be 524288 (512 KiB),
matching jemalloc's default `lg_prof_sample=19`. The interval MUST
be greater than zero.

## Sampling

r[pprof.geometric-sampling]
Sampling MUST use the geometric distribution to amortise RNG cost.
Each thread maintains a byte counter initialised from a geometric
draw with parameter `1/R` (where `R` is the sample interval). The
counter is decremented by the allocation size on each alloc. When
the counter reaches zero or goes negative, a sample is recorded and
the counter is reinitialised from a fresh geometric draw.

r[pprof.fast-path-cost]
On the allocation fast path, sampling MUST cost at most one integer
subtract and one branch (counter check). No function calls, locks,
or RNG on the fast path when the counter is positive.

r[pprof.inactive-cost]
When profiling is compiled in but inactive at runtime, the fast path
MUST NOT perform the counter decrement. The cost MUST be a single
branch on a flag (or atomic load).

## Reentrancy

r[pprof.no-reentrant-sample]
The profiling instrumentation (sample recording, side-table updates,
stack capture) MUST NOT re-enter the profiling path when its own
internal allocations trigger the allocator. A thread-local reentrancy
guard MUST suppress sampling during profiling operations.

## Stack traces

r[pprof.backtrace]
When a sample is triggered, the allocator MUST capture the current
call stack (backtrace). The depth limit SHOULD be configurable with
a default of 64 frames.

r[pprof.backtrace-dedup]
Captured stack traces MUST be deduplicated. Identical traces share
a single `StackId`. The deduplication table MUST be thread-safe
(samples from different threads may produce the same trace).

## Sample records

r[pprof.sample-record]
Each sampled allocation MUST record: the `StackId`, the allocation
size in bytes, and the sampling interval `R` active at sample time.

r[pprof.unbiased-weight]
Each sample's contribution to reported totals MUST be unbiased using
the per-byte Poisson model. The weight for an allocation of size `Z`
sampled at rate `R` is `Z / (1 - e^{-Z/R})`. Unbiasing MUST be
applied per-sample (not after aggregation across samples).

r[pprof.live-tracking]
The allocator MUST maintain a mapping from sampled allocation pointer
to its sample record (side table). This enables live-heap attribution
on dealloc. The side table MUST be thread-safe.

r[pprof.free-decrement]
When a sampled allocation is freed, the allocator MUST look up and
remove its entry from the side table, decrementing live counters for
the associated stack trace. The lookup MUST NOT add more than O(1)
amortised cost to the dealloc path for non-sampled allocations.

## Per-size-class attribution

r[pprof.class-label]
Each sample MUST be tagged with the size class index used to service
the allocation. This MUST appear as a pprof label (`size_class`) on
the sample, enabling filtering and grouping by size class in pprof
tooling (e.g. `pprof -tags`, `pprof -tagfocus`).

## Dump

r[pprof.dump-api]
The allocator MUST expose a function to dump the current heap profile.
The dump MUST be callable from any thread without stopping the allocator.
In-flight allocations during the dump MAY be missed.

r[pprof.dump-format]
The dump MUST produce output in the pprof protobuf format
(`perftools.profiles.Profile`), gzip-compressed, suitable for
consumption by `go tool pprof`, jeprof, and similar tools.

r[pprof.sample-types]
The profile MUST expose four sample types:
- `alloc_objects`: cumulative count of sampled allocations (unbiased).
- `alloc_space`: cumulative bytes of sampled allocations (unbiased).
- `inuse_objects`: count of sampled allocations still live (unbiased).
- `inuse_space`: bytes of sampled allocations still live (unbiased).
