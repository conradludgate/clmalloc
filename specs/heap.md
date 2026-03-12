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
Allocation MUST first attempt to pop from the active slab's local free list
(no atomics). If the local list is empty, the heap MUST drain the slab's
remote free list before requesting a new slab.

r[heap.slab-request]
When the active slab is fully allocated and its remote free list is also
empty, the heap MUST request a new slab from the global page pool and make
it the active slab for that size class.

r[heap.page-queue]
Non-active slabs MUST be tracked in two per-class doubly-linked queues:
a full queue (exhausted slabs awaiting remote frees) and a partial queue
(slabs with known free slots). Each slab carries a back-pointer to its
predecessor's next-link, enabling O(1) removal from any position. When
the active slab is exhausted, the heap MUST first try to pop from the
partial queue (O(1)). If the partial queue is empty, the heap MUST scan
the full queue, draining remote frees on each slab, and move all discovered
partial slabs to the partial queue in one pass.

r[heap.dealloc-promote]
When a local deallocation causes a full-queue slab to gain its first free
slot, the heap MUST promote it to the partial queue (or make it active)
in O(1) using the doubly-linked list back-pointer.

r[heap.dealloc-o1]
Deallocation MUST be O(1). If the pointer belongs to any slab owned by this
heap, the heap MUST push directly to the slab's local free list (no atomics)
and perform any resulting list promotion in O(1). If the slab belongs to a
different heap, the heap MUST push to the slab's remote free list (one
atomic CAS). The heap MUST NOT walk any slab queue or batch frees during
deallocation.

## Cleanup

r[heap.thread-exit]
When a thread exits, its heap MUST be cleaned up. Slabs with all-free slots
MUST be returned to the global page pool. Slabs with outstanding allocations
MUST remain valid (they will be freed via remote free lists by other threads).

r[heap.abandon]
When a thread exits with slabs that still have outstanding allocations, those
slabs MUST be placed on a global abandoned-slab list. Other heaps MUST adopt
abandoned slabs for the same size class before requesting a fresh slab from
the page pool. Once all slots in an abandoned slab are freed, it MUST be
returned to the page pool.
