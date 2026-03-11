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
When a slab is returned to the pool free list and the segment will NOT
be fully purged, the pool MUST release the slab's physical pages back
to the OS via `PageAllocator::purge`. The virtual address range is
retained so the slab can be reused without a new mmap. This reduces
RSS when partially-occupied segments prevent full segment munmap.

r[pool.purge-before-publish]
The pool MUST purge a slab's physical pages BEFORE placing it on the
free list. If the slab were published first, another thread could pop
it, reinitialize its header for a new size class, and begin allocating
from it while the purge is still in flight — zeroing the new header
and corrupting live metadata.

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
