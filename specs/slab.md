# Slab Management

Slabs are contiguous memory regions that serve allocations of a single size
class. Each slab is owned by a thread and contains a local free list and
an atomic remote free list for cross-thread deallocation.

## Structure

r[slab.alignment]
Each slab MUST be aligned to its own size so that the slab base address can
be recovered from any interior pointer by masking off the low bits.

r[slab.single-class]
Each slab MUST serve exactly one size class. All slots within a slab are
the same size.

r[slab.metadata]
Slab metadata (size class, free lists) MUST be stored at a fixed offset
within the slab (e.g. the slab header) so it can be located in O(1) from
any pointer within the slab.

r[slab.owner]
Each slab MUST have exactly one owner. Ownership MUST be expressed through
handle types: an owner handle (`Slab`) that requires exclusive access for
local operations, and a shared handle (`SlabRef`) that only permits atomic
remote deallocation. Determining whether a deallocation is local or remote
is the responsibility of the heap layer.

## Local free list

r[slab.local-freelist]
The local free list MUST be a singly-linked intrusive list embedded in the
free slots themselves. Allocation pops the head; local deallocation pushes
to the head.

r[slab.local-no-atomics]
Operations on the local free list MUST NOT use atomic instructions. Only
the owning thread accesses it.

## Remote free list

r[slab.remote-freelist]
The remote free list MUST be an atomic singly-linked intrusive list. Remote
threads push freed pointers using compare-and-swap.

r[slab.remote-drain]
When the local free list is empty, the owning thread MUST atomically swap
the remote free list to null and adopt the entire chain as the new local
free list.

## Lifecycle

r[slab.alloc-from-pool]
New slabs MUST be obtained from a global page pool backed by OS memory
(mmap with MAP_ANONYMOUS).

r[slab.return-to-pool]
When all slots in a slab are free (local + remote lists account for every
slot), the slab MUST be returned to the global page pool for reuse by
any thread.
