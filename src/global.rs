//! GlobalAlloc implementation wiring the thread-local heap to Rust's
//! allocator interface.
//!
//! Thread-local heap access uses `pthread_key_create` / `pthread_getspecific`
//! instead of Rust's `thread_local!` macro, because global allocators must
//! not register Rust TLS destructors (the runtime aborts if they do).

#[cfg(unix)]
mod imp {
    use core::alloc::{GlobalAlloc, Layout};
    use core::mem::size_of;
    use core::ptr::{self, NonNull};
    use std::sync::Once;

    use crate::heap::Heap;
    use crate::pool::PagePool;
    use crate::size_class;
    use crate::slab::SlabRef;
    use crate::sys::MmapAllocator;

    type HeapTy = Heap<'static, MmapAllocator>;

    /// Sentinel stored via `pthread_setspecific` after the destructor runs,
    /// preventing heap re-creation during late TLS teardown.
    const SENTINEL: *mut HeapTy = std::ptr::dangling_mut::<HeapTy>();

    static KEY_ONCE: Once = Once::new();
    static mut PTHREAD_KEY: libc::pthread_key_t = 0;

    // r[impl alloc.tls-pthread-cleanup]
    unsafe extern "C" fn heap_destructor(ptr: *mut libc::c_void) {
        if !ptr.is_null() && ptr != SENTINEL as *mut libc::c_void {
            unsafe { ptr::drop_in_place(ptr as *mut HeapTy) };
            unsafe { libc::free(ptr) };
        }
        // Prevent re-creation if other TLS destructors call dealloc.
        unsafe { libc::pthread_setspecific(PTHREAD_KEY, SENTINEL as *mut libc::c_void) };
    }

    fn ensure_key() {
        KEY_ONCE.call_once(|| {
            let ret = unsafe { libc::pthread_key_create(&raw mut PTHREAD_KEY, Some(heap_destructor)) };
            assert_eq!(ret, 0, "pthread_key_create failed");
        });
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

        // r[impl alloc.thread-local] r[impl alloc.tls-no-destructor]
        #[inline]
        fn get_heap(&'static self) -> *mut HeapTy {
            ensure_key();
            let ptr = unsafe { libc::pthread_getspecific(PTHREAD_KEY) } as *mut HeapTy;
            if ptr.is_null() {
                return self.init_heap();
            }
            ptr
        }

        #[cold]
        fn init_heap(&'static self) -> *mut HeapTy {
            let ptr = unsafe { libc::malloc(size_of::<HeapTy>()) } as *mut HeapTy;
            if ptr.is_null() {
                return ptr;
            }
            unsafe { ptr::write(ptr, Heap::new(&self.pool)) };
            unsafe { libc::pthread_setspecific(PTHREAD_KEY, ptr as *mut libc::c_void) };
            ptr
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
            let heap = this.get_heap();
            if heap == SENTINEL || heap.is_null() {
                return ptr::null_mut();
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
            // SAFETY: ClMalloc is only used as #[global_allocator] in a static.
            let this: &'static Self = unsafe { &*(self as *const Self) };
            let heap = this.get_heap();
            if heap != SENTINEL && !heap.is_null() {
                unsafe { (*heap).dealloc(NonNull::new_unchecked(ptr), layout) };
                return;
            }
            // Post-destructor fallback: heap is gone, use remote dealloc directly.
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
