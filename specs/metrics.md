# Metrics

Allocation metrics for observability and production debugging. Designed
to be cheaply maintained (thread-local counters, no locks on the hot path)
and queryable without stopping the allocator.

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

## Global aggregation

r[metrics.global-snapshot]
The allocator MUST provide a function that aggregates metrics across all
thread-local heaps into a consistent snapshot. This MAY briefly lock or
iterate over heap registrations.

r[metrics.global-resident]
The global snapshot MUST include total resident memory (bytes obtained
from the OS via mmap and not yet returned).

r[metrics.global-active]
The global snapshot MUST include total active bytes (sum of per-thread
active bytes).

## Per-size-class stats

r[metrics.class-alloc-count]
Each thread-local heap SHOULD maintain a per-size-class allocation count.

r[metrics.class-slab-count]
The global snapshot SHOULD include the number of active slabs per size class.

## Remote free tracking

r[metrics.remote-free-count]
Each thread-local heap SHOULD maintain a count of remote frees received
(drains from the remote free list). This indicates cross-thread traffic
and is useful for diagnosing work-stealing allocation patterns.
