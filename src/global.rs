//! GlobalAlloc implementation wiring the thread-local heap to Rust's
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

    thread_local! {
        /// Fast-path pointer: null = uninit, DESTROYED = torn down,
        /// otherwise points to heap-allocated Heap.
        static HEAP: Cell<*mut HeapTy> = const { Cell::new(ptr::null_mut()) };
    }

    // r[impl alloc.no-reentrant-init]
    /// Lazy pthread key for destructor registration. `spin::Once` spins on
    /// contention instead of parking, so it never allocates — safe for use
    /// inside a global allocator.
    static PTHREAD_KEY: spin::Once<libc::pthread_key_t> = spin::Once::new();

    // r[impl alloc.tls-pthread-cleanup]
    unsafe extern "C" fn heap_destructor(ptr: *mut libc::c_void) {
        if ptr.is_null() || ptr == DESTROYED as *mut libc::c_void {
            return;
        }
        #[cfg(feature = "metrics")]
        unsafe {
            let heap = ptr as *const HeapTy;
            let metrics_ptr =
                core::cell::UnsafeCell::raw_get(core::ptr::addr_of!((*heap).metrics)) as *const _;
            (*heap).pool().deregister_heap(metrics_ptr);
        }
        unsafe { ptr::drop_in_place(ptr as *mut HeapTy) };
        unsafe { libc::free(ptr) };
        HEAP.with(|c| c.set(DESTROYED));
    }

    fn ensure_key() -> libc::pthread_key_t {
        *PTHREAD_KEY.call_once(|| {
            let mut key: libc::pthread_key_t = 0;
            let ret = unsafe { libc::pthread_key_create(&mut key, Some(heap_destructor)) };
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
        pub const fn new() -> Self {
            Self {
                pool: PagePool::new(MmapAllocator),
            }
        }

        // r[impl alloc.thread-local]
        #[cold]
        #[inline(never)]
        fn init_heap(&'static self) -> *mut HeapTy {
            let ptr = unsafe { libc::malloc(size_of::<HeapTy>()) } as *mut HeapTy;
            if ptr.is_null() {
                return ptr;
            }
            unsafe { ptr::write(ptr, Heap::new(&self.pool)) };
            #[cfg(feature = "metrics")]
            self.pool.register_heap(unsafe {
                core::cell::UnsafeCell::raw_get(core::ptr::addr_of!((*ptr).metrics)) as *const _
            });
            HEAP.with(|c| c.set(ptr));
            let key = ensure_key();
            unsafe { libc::pthread_setspecific(key, ptr as *mut libc::c_void) };
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
            let this: &'static Self = unsafe { &*(self as *const Self) };
            let mut heap = HEAP.with(|c| c.get());
            if heap == DESTROYED {
                return ptr::null_mut();
            }
            if heap.is_null() {
                heap = this.init_heap();
                if heap.is_null() {
                    return ptr::null_mut();
                }
            }
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
            let heap = HEAP.with(|c| c.get());
            if !heap.is_null() && heap != DESTROYED {
                unsafe { (*heap).dealloc(NonNull::new_unchecked(ptr), layout) };
                return;
            }
            // r[impl alloc.post-exit-dealloc]
            // SAFETY: ClMalloc is only used as #[global_allocator] in a static.
            let this: &'static Self = unsafe { &*(self as *const Self) };
            if let Some(_idx) = size_class::class_index(layout) {
                let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr) };
                slab_ref.dealloc_remote(unsafe { NonNull::new_unchecked(ptr) });
            } else {
                unsafe { this.pool.dealloc_large(NonNull::new_unchecked(ptr), layout) };
            }
        }
    }

    unsafe impl Send for ClMalloc {}
    unsafe impl Sync for ClMalloc {}
}

#[cfg(unix)]
pub use imp::ClMalloc;
