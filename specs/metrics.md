# Metrics

Allocation metrics for observability and production debugging. Designed
to be cheaply maintained (thread-local counters, no locks on the hot path)
and queryable without stopping the allocator.

Per-size-class counters use size class boundaries as bucket boundaries,
mapping directly to Prometheus histogram exposition format.

## Thread-local counters

r[metrics.thread-alloc-bytes]
Each thread-local heap MUST maintain a cumulative count of bytes allocated.
This counter MUST be updated on every allocation without atomic operations.

r[metrics.thread-free-bytes]
Each thread-local heap MUST maintain a cumulative count of bytes freed
(both local and remote frees attributed to the freeing thread). This
counter MUST be updated on every deallocation without atomic operations.

r[metrics.thread-active-bytes]
Each thread-local heap MUST expose the current active (allocated minus freed)
byte count, derived from the alloc and free counters.

## Global gauges

r[metrics.global-snapshot]
The allocator MUST provide a function that aggregates metrics across all
thread-local heaps into a consistent snapshot. This MAY briefly lock or
iterate over heap registrations.

r[metrics.global-allocated]
The global snapshot MUST include total allocated bytes: the sum of
application-requested allocation sizes currently outstanding (alloc
minus free across all heaps). Corresponds to jemalloc `stats.allocated`.

r[metrics.global-active]
The global snapshot MUST include total active bytes: the bytes in
slab pages that contain at least one outstanding allocation, plus
the live large-allocation footprint. Computed as
`outstanding_slabs * SLAB_SIZE + (large_alloc_bytes - large_dealloc_bytes)`.
`active >= allocated`. Corresponds to jemalloc `stats.active`.

r[metrics.global-mapped]
The global snapshot MUST include total mapped bytes: the sum of all
virtual address space obtained from the OS via mmap (or equivalent)
and not yet returned via munmap. This MUST include both segment
memory and large allocations. Corresponds to jemalloc `stats.mapped`.

r[metrics.global-resident]
The global snapshot MUST include total resident bytes: the portion
of mapped memory that is physically backed by RAM (not paged out
or advised away). Corresponds to jemalloc `stats.resident`.

r[metrics.global-metadata]
The global snapshot MUST include metadata bytes: memory consumed by
the allocator's own bookkeeping (slab headers, pool state, segment
tracking arrays, side tables). Corresponds to jemalloc `stats.metadata`.

r[metrics.global-retained]
The global snapshot MUST include retained bytes: virtual address
space that has been purged (e.g. via `madvise(MADV_DONTNEED)`) but
not yet returned to the OS via munmap. These pages are available for
reuse without a new mmap. Corresponds to jemalloc `stats.retained`.

## Per-size-class counters

Size class boundaries serve as Prometheus histogram bucket boundaries.
Counters are monotonic, enabling `rate()` and `histogram_quantile()`
queries.

r[metrics.class-alloc-count]
Each thread-local heap MUST maintain a per-size-class cumulative
allocation count (objects). Updated without atomics.

r[metrics.class-dealloc-count]
Each thread-local heap MUST maintain a per-size-class cumulative
deallocation count (objects). Updated without atomics.

r[metrics.class-alloc-bytes]
Each thread-local heap MUST maintain per-size-class cumulative
bytes allocated.

r[metrics.class-free-bytes]
Each thread-local heap MUST maintain per-size-class cumulative
bytes freed.

r[metrics.class-live]
The global snapshot MUST include per-size-class live object count
and live bytes, derived from the difference of alloc and dealloc
counters across all heaps.

r[metrics.class-slab-count]
The global snapshot MUST include the number of active slabs per
size class (sum of active, full, and partial queue slabs across
all heaps).

r[metrics.histogram-storage]
Per-size-class counters MUST be stored as independent per-bucket
values (not cumulative). This avoids multi-bucket atomic updates
on each allocation. Cumulative `le`-style sums MUST be computed
at exposition time, not at increment time.

r[metrics.histogram-exposition]
At exposition time, the allocator MUST produce Prometheus-compatible
cumulative histogram output from the per-bucket counters by computing
a rolling sum over size classes in ascending order. Each size class's
upper bound (the class size) serves as the `le` label value. A `+Inf`
bucket MUST include large allocations that exceed the maximum size
class. The `_sum` MUST equal total bytes and `_count` MUST equal
total objects.

## Cache effectiveness

r[metrics.cache-hit-count]
Each thread-local heap MUST maintain a per-size-class cumulative count
of allocations served directly from the free cache (tcache). Updated
without atomics. A low hit rate relative to total allocs indicates the
cache is not absorbing enough reuse.

r[metrics.cache-flush-count]
Each thread-local heap MUST maintain a per-size-class cumulative count
of cache flush events (cache overflow causing a batch return to slabs).
A high flush rate relative to alloc rate indicates `CACHE_CAP` may be
too small.

## Thread churn

r[metrics.abandon-count]
The page pool MUST maintain a cumulative count of slabs abandoned
(thread exit with outstanding allocations), broken down by size class.

r[metrics.adopt-count]
The page pool MUST maintain a cumulative count of slabs adopted
(a new heap reclaims an abandoned slab), broken down by size class.

## OS interaction

r[metrics.segment-mmap-count]
The page pool MUST maintain a cumulative count of segment mmap calls.

r[metrics.segment-munmap-count]
The page pool MUST maintain a cumulative count of segment munmap calls
(purges). A high munmap rate may indicate the purge threshold is too
aggressive.

r[metrics.slab-purge-count]
The page pool MUST maintain a cumulative count of slab purge operations
(madvise calls on free slabs). This tracks how often the allocator
releases physical pages from individual slabs without unmapping the
entire segment.

## Large allocations

r[metrics.large-alloc-count]
Each thread-local heap MUST maintain a cumulative count of allocations
that exceed the maximum size class and bypass the slab system.

r[metrics.large-alloc-bytes]
Each thread-local heap MUST maintain cumulative bytes for large
allocations. These go directly to mmap and are significantly more
expensive than slab-served allocations.

r[metrics.large-dealloc-bytes]
Each thread-local heap MUST maintain cumulative bytes for large
deallocations. Combined with `large-alloc-bytes`, this yields the
live large-allocation footprint needed for the `active` gauge.

## Remote free tracking

r[metrics.remote-free-count]
Each thread-local heap MUST maintain a count of remote frees received
(drains from the remote free list). This indicates cross-thread traffic
and is useful for diagnosing work-stealing allocation patterns.

## Pool contention

r[metrics.pool-lock-count]
The page pool MUST maintain a cumulative count of mutex acquisitions.
Together with segment mmap/munmap counts, this separates lock
contention cost from syscall cost in the pool slow path.
