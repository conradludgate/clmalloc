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

## Thread-local access

r[alloc.tls-no-destructor]
The global allocator MUST NOT register Rust thread-local destructors (via
`thread_local!` with `Drop` types). The Rust runtime aborts if a global
allocator registers TLS destructors during allocation.

r[alloc.tls-pthread-cleanup]
Thread-exit cleanup MUST use `pthread_key_create` (or platform equivalent)
to register a destructor outside Rust's TLS infrastructure.

## Reentrancy

r[alloc.no-reentrant-init]
Initialization of global allocator state (e.g. TLS key creation) MUST NOT
use mechanisms that may allocate through the global allocator (such as
`std::sync::Once`, which may allocate to park contending threads). Lock-free
atomic CAS or similar allocation-free synchronization MUST be used instead.

## Zero-size allocations

r[alloc.zst]
Zero-sized layouts MUST be handled without accessing the underlying allocator.
`alloc` MUST return a valid non-null dangling pointer and `dealloc` MUST be
a no-op.
