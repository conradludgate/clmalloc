# Global Page Pool

The page pool manages OS-level memory and distributes slabs to thread-local
heaps.

## Slab provisioning

r[pool.alloc-slab]
The page pool MUST provide slabs of the requested size, properly aligned
to the slab size boundary (for pointer masking).

r[pool.thread-safe]
The page pool MUST be safe to access from any thread. It SHOULD use a
low-contention lock (e.g. spin lock). Lock-free pop from a shared slab
cache is not viable due to data races on intrusive next-pointers; this
is consistent with jemalloc, tcmalloc, and mimalloc which all use locks
for their shared page/extent caches.

## OS memory

r[pool.mmap]
The page pool MUST obtain memory from the OS through a pluggable page
allocator trait. The production implementation MUST use mmap (or platform
equivalent) with MAP_ANONYMOUS | MAP_PRIVATE.

r[pool.batch-mmap]
The page pool SHOULD request memory from the OS in large batches (e.g.
2 MiB or more) and carve slabs from the batch, to amortize syscall cost.

## Large allocations

r[pool.large-alloc]
Allocations exceeding the maximum size class MUST be satisfied by a
dedicated mmap call, not from the slab pool.

r[pool.large-dealloc]
Large allocations MUST be returned to the OS via munmap on deallocation.
