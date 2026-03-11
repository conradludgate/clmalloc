# GlobalAlloc Interface

The top-level allocation interface implementing Rust's `GlobalAlloc` trait.

## Allocation

r[alloc.layout]
`alloc` MUST return a pointer aligned to at least `layout.align()`, pointing
to a region of at least `layout.size()` usable bytes.

r[alloc.null-on-failure]
`alloc` MUST return a null pointer if the allocation cannot be satisfied.

r[alloc.size-class-dispatch]
`alloc` MUST round the requested size up to the corresponding size class
and allocate from that class's free list.

r[alloc.thread-local]
`alloc` MUST first attempt to satisfy the request from the calling thread's
local heap without acquiring any locks.

## Deallocation

r[dealloc.layout-trusted]
`dealloc` MUST use the provided `Layout` to determine the size class. It
MUST NOT read metadata headers from the allocation to recover the size.

r[dealloc.local-fast-path]
When the slab owning the pointer belongs to the calling thread, `dealloc`
MUST push the pointer onto the slab's local free list without atomic operations.

r[dealloc.remote-path]
When the slab owning the pointer belongs to a different thread, `dealloc`
MUST push the pointer onto the slab's atomic remote free list using a
lock-free compare-and-swap loop.

## Zero-size allocations

r[alloc.zst]
Zero-sized layouts MUST be handled without accessing the underlying allocator.
`alloc` MUST return a valid non-null dangling pointer and `dealloc` MUST be
a no-op.
