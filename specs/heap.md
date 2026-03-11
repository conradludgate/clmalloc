# Thread-Local Heap

Each thread has a local heap that provides fast, contention-free allocation.

## Initialization

r[heap.thread-local]
Each thread MUST have its own heap instance, accessed without locks.
The heap MUST be initialized lazily on first allocation.

r[heap.identity]
Each heap MUST have a unique identifier used to determine slab ownership
during deallocation.

## Per-class state

r[heap.class-bins]
The heap MUST maintain one bin per size class. Each bin holds a pointer to
the currently active slab for that class.

r[heap.alloc-fast-path]
Allocation MUST first attempt to pop from the active slab's local free list.
If the local list is empty, the heap MUST drain the slab's remote free list
before requesting a new slab.

r[heap.slab-request]
When the active slab is fully allocated and its remote free list is also
empty, the heap MUST request a new slab from the global page pool and make
it the active slab for that size class.

## Cleanup

r[heap.thread-exit]
When a thread exits, its heap MUST be cleaned up. Slabs with all-free slots
MUST be returned to the global page pool. Slabs with outstanding allocations
MUST remain valid (they will be freed via remote free lists by other threads).

r[heap.abandon]
When a thread exits with slabs that still have outstanding allocations, those
slabs MUST be placed on a global abandoned-slab list. Other heaps SHOULD adopt
abandoned slabs for the same size class before requesting a fresh slab from
the page pool. Once all slots in an abandoned slab are freed, it MUST be
returned to the page pool.
