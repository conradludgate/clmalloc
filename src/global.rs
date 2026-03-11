//! `GlobalAlloc` implementation wiring the thread-local heap to Rust's
//! allocator interface.
//!
//! A `Cell<*mut HeapTy>` provides the fast-path pointer (single TLS load).
//! The Heap itself is allocated via `libc::malloc` so its lifetime is
//! independent of the TLS block (which macOS may free before pthread
//! destructors run). A pthread key is registered solely for destructor
//! callback on thread exit.

#[cfg(unix)]
mod imp {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::Cell;
    use core::mem::size_of;
    use core::ptr::{self, NonNull};

    use crate::heap::Heap;
    use crate::pool::PagePool;
    use crate::size_class;
    use crate::slab::SlabRef;
    use crate::sys::MmapAllocator;

    type HeapTy = Heap<'static, MmapAllocator>;

    /// Stored in the HEAP Cell after the destructor runs, preventing
    /// heap re-creation during late TLS teardown. Distinguished from
    /// null (which means "not yet initialized").
    const DESTROYED: *mut HeapTy = ptr::dangling_mut::<HeapTy>();

    // r[impl alloc.tls-no-destructor]
    thread_local! {
        /// Fast-path pointer: null = uninit, DESTROYED = torn down,
        /// otherwise points to heap-allocated Heap.
        /// `Cell<*mut T>` has no Drop, so no TLS destructor is registered.
        static HEAP: Cell<*mut HeapTy> = const { Cell::new(ptr::null_mut()) };
    }

    // r[impl alloc.no-reentrant-init]
    /// Lazy pthread key for destructor registration. `spin::Once` spins on
    /// contention instead of parking, so it never allocates — safe for use
    /// inside a global allocator.
    static PTHREAD_KEY: spin::Once<libc::pthread_key_t> = spin::Once::new();

    // r[impl alloc.tls-pthread-cleanup]
    unsafe extern "C" fn heap_destructor(ptr: *mut libc::c_void) {
        if ptr.is_null() || ptr == DESTROYED.cast::<libc::c_void>() {
            return;
        }
        #[cfg(feature = "metrics")]
        // SAFETY: The heap is alive; pool() returns the static pool reference.
        // UnsafeCell::raw_get obtains the inner pointer without creating an intermediate reference.
        unsafe {
            let heap = ptr.cast::<HeapTy>();
            let metrics_ptr =
                core::cell::UnsafeCell::raw_get(core::ptr::addr_of!((*heap).metrics)).cast_const();
            (*heap).pool().deregister_heap(metrics_ptr);
        }
        // SAFETY: The heap was initialized in init_heap and has not been dropped yet.
        // Dropping flushes caches and returns slabs to the pool.
        unsafe { ptr::drop_in_place(ptr.cast::<HeapTy>()) };
        // SAFETY: The pointer was allocated by libc::malloc in init_heap.
        unsafe { libc::free(ptr) };
        HEAP.with(|c| c.set(DESTROYED));
    }

    fn ensure_key() -> libc::pthread_key_t {
        *PTHREAD_KEY.call_once(|| {
            let mut key: libc::pthread_key_t = 0;
            // SAFETY: key is a valid pointer to a local variable. heap_destructor has the correct
            // signature for a pthread destructor.
            let ret = unsafe { libc::pthread_key_create(&raw mut key, Some(heap_destructor)) };
            assert_eq!(ret, 0, "pthread_key_create failed");
            key
        })
    }

    pub struct ClMalloc {
        pool: PagePool<MmapAllocator>,
    }

    impl Default for ClMalloc {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ClMalloc {
        #[must_use]
        pub const fn new() -> Self {
            Self {
                pool: PagePool::new(MmapAllocator),
            }
        }

        // r[impl alloc.thread-local]
        #[cold]
        #[inline(never)]
        fn init_heap(&'static self) -> *mut HeapTy {
            // SAFETY: Allocating size_of::<HeapTy>() bytes from the system allocator (not our
            // allocator, avoiding reentrancy).
            let ptr = unsafe { libc::malloc(size_of::<HeapTy>()) }.cast::<HeapTy>();
            if ptr.is_null() {
                return ptr;
            }
            // SAFETY: ptr is non-null (checked above), properly aligned (malloc guarantees
            // max_align_t alignment), and points to uninitialized memory of sufficient size.
            unsafe { ptr::write(ptr, Heap::new(&self.pool)) };
            #[cfg(feature = "metrics")]
            self.pool.register_heap(
                // SAFETY: ptr was just initialized; addr_of! computes the metrics field address
                // without creating an intermediate reference to the partially-initialized Heap.
                unsafe {
                    core::cell::UnsafeCell::raw_get(core::ptr::addr_of!((*ptr).metrics))
                        .cast_const()
                },
            );
            HEAP.with(|c| c.set(ptr));
            let key = ensure_key();
            // SAFETY: key is a valid pthread key and ptr is a valid heap pointer.
            unsafe { libc::pthread_setspecific(key, ptr.cast::<libc::c_void>()) };
            ptr
        }

        #[cfg(feature = "metrics")]
        pub fn snapshot(&'static self) -> crate::metrics::MetricsSnapshot {
            self.pool.snapshot()
        }

        // r[impl pprof.activate]
        #[cfg(feature = "pprof")]
        pub fn set_prof_active(&self, active: bool) {
            crate::pprof::set_prof_active(active);
        }

        // r[impl pprof.dump-api]
        #[cfg(feature = "pprof")]
        /// # Errors
        ///
        /// Returns an error if writing the profile to `writer` fails.
        pub fn dump_heap_profile(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()> {
            crate::pprof::dump(writer)
        }
    }

    // r[impl alloc.layout] r[impl alloc.null-on-failure] r[impl alloc.size-class-dispatch]
    // r[impl dealloc.layout-trusted] r[impl dealloc.local-fast-path] r[impl dealloc.remote-path]
    unsafe impl GlobalAlloc for ClMalloc {
        // r[impl alloc.zst]
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if layout.size() == 0 {
                return layout.align() as *mut u8;
            }
            // SAFETY: ClMalloc is only used as #[global_allocator] in a static.
            let this: &'static Self = unsafe { &*ptr::from_ref(self) };
            let mut heap = HEAP.with(Cell::get);
            if heap == DESTROYED {
                return ptr::null_mut();
            }
            if heap.is_null() {
                heap = this.init_heap();
                if heap.is_null() {
                    return ptr::null_mut();
                }
            }
            // SAFETY: heap is non-null, non-DESTROYED, and points to an initialized Heap.
            match unsafe { (*heap).alloc(layout) } {
                Some(ptr) => ptr.as_ptr(),
                None => ptr::null_mut(),
            }
        }

        // r[impl alloc.zst]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if layout.size() == 0 {
                return;
            }
            let heap = HEAP.with(Cell::get);
            if !heap.is_null() && heap != DESTROYED {
                // SAFETY: heap is non-null and non-DESTROYED, pointing to an initialized Heap.
                // ptr is non-null per GlobalAlloc contract.
                unsafe { (*heap).dealloc(NonNull::new_unchecked(ptr), layout) };
                return;
            }
            // r[impl alloc.post-exit-dealloc]
            // SAFETY: ClMalloc is only used as #[global_allocator] in a static.
            let this: &'static Self = unsafe { &*ptr::from_ref(self) };
            if let Some(_idx) = size_class::class_index(layout) {
                // SAFETY: ptr was allocated from a slab (class_index returned Some), so it's a
                // valid interior pointer.
                let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr) };
                // SAFETY: ptr is non-null per GlobalAlloc::dealloc contract.
                slab_ref.dealloc_remote(unsafe { NonNull::new_unchecked(ptr) });
            } else {
                // SAFETY: ptr is non-null per GlobalAlloc contract. The layout matches the
                // original allocation.
                unsafe { this.pool.dealloc_large(NonNull::new_unchecked(ptr), layout) };
            }
        }
    }

    // SAFETY: ClMalloc contains only a PagePool which is protected by internal spin locks.
    // All mutable state is behind synchronization primitives.
    unsafe impl Send for ClMalloc {}
    unsafe impl Sync for ClMalloc {}
}

#[cfg(unix)]
pub use imp::ClMalloc;
