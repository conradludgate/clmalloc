# Global Page Pool

The page pool manages OS-level memory and distributes slabs to thread-local
heaps.

## Slab provisioning

r[pool.alloc-slab]
The page pool MUST provide slabs of the requested size, properly aligned
to the slab size boundary (for pointer masking).

r[pool.thread-safe]
The page pool MUST be safe to access from any thread. It MUST use a
low-contention lock (e.g. spin lock). Lock-free pop from a shared slab
cache is not viable due to data races on intrusive next-pointers; this
is consistent with jemalloc, tcmalloc, and mimalloc which all use locks
for their shared page/extent caches.

## OS memory

r[pool.mmap]
The page pool MUST obtain memory from the OS through a pluggable page
allocator trait. The production implementation MUST use mmap (or platform
equivalent) with MAP_ANONYMOUS | MAP_PRIVATE.

r[sys.purge-pages]
The page allocator trait MUST provide a `purge` method that releases
physical pages back to the OS without unmapping the virtual address
range. On Linux this MUST use `madvise(MADV_DONTNEED)`. On macOS this
MUST use `madvise(MADV_FREE)`. Subsequent reads from purged memory
MUST return zeroes. The default implementation MAY be a no-op (e.g.
for test backends).

r[pool.batch-mmap]
The page pool MUST request memory from the OS in large batches (e.g.
2 MiB or more) and carve slabs from the batch, to amortize syscall cost.

r[pool.no-syscall-under-lock]
The page pool MUST NOT perform OS memory allocation (mmap) while holding
the pool lock. The lock MUST be released before the syscall and
re-acquired after, so that other threads can pop from the free list
without blocking on a ~10μs mmap.

## Memory return

r[pool.purge]
When a segment has all of its slabs on the free list, the page pool MUST
return the physical pages to the OS (e.g. via `madvise(MADV_DONTNEED)` or
`munmap`). This ensures that long-running processes release memory when
usage drops.

r[pool.purge-free-slab]
The pool MUST eventually purge or reuse every slab returned to it.
Purging MAY be deferred to amortize syscall cost.

r[pool.deferred-purge]
The pool MUST maintain two tiers of free slabs: a dirty list (physical
pages still resident) and a clean list (pages purged via
`PageAllocator::purge`). Returned slabs go onto the dirty list.
When the dirty count exceeds a high-water mark, the pool MUST purge
slabs in batch until the count drops to a low-water mark. Purging
MUST happen outside the pool lock: pop slabs into a thread-local
buffer, release the lock, purge each slab, re-acquire the lock, and
push them onto the clean list.

r[pool.dirty-reuse]
The pool MAY hand out dirty slabs without purging. `Slab::init`
overwrites the header unconditionally, so stale page contents do not
affect correctness.

r[pool.purge-not-on-shared-list]
The pool MUST NOT purge a slab while it is visible on any shared list.
Purging MUST only happen on slabs held in a thread-local buffer after
being popped from the dirty list. This prevents a concurrent
`alloc_slab` from handing out a slab whose pages are being zeroed.

## Exhaustion

r[pool.no-panic-under-lock]
Operations that hold the pool lock MUST NOT panic. If a capacity limit is
reached, the operation MUST return `None` to the caller. Panicking while
holding the lock causes deadlock because the panic handler allocates.

## Large allocations

r[pool.large-alloc]
Allocations exceeding the maximum size class MUST be satisfied by a
dedicated mmap call, not from the slab pool.

r[pool.large-dealloc]
Large allocations MUST be returned to the OS via munmap on deallocation.
