//! OS memory abstraction.
//!
//! The [`PageAllocator`] trait decouples the page pool from the OS backend,
//! allowing tests (and Miri) to substitute a system-allocator-based
//! implementation that doesn't require syscalls.

use core::alloc::Layout;
use core::ptr::NonNull;

/// Contract for a page-level memory backend.
///
/// # Safety
///
/// Implementations must guarantee:
/// - Returned memory satisfies `layout` (size and alignment).
/// - Returned memory is zeroed.
/// - `dealloc` is only called with pointers previously returned by `alloc`,
///   with the same `layout`.
pub unsafe trait PageAllocator {
    fn alloc(&self, layout: Layout) -> Option<NonNull<u8>>;

    /// # Safety
    /// `ptr` must have been returned by `alloc` with the same `layout`.
    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout);

    /// Release physical pages back to the OS without releasing the virtual
    /// address range. The memory remains accessible (reads return zeroes)
    /// but the OS may reclaim the physical frames.
    ///
    /// No-op by default; overridden by `MmapAllocator`.
    ///
    /// # Safety
    /// `ptr` and `len` must describe a page-aligned region within a
    /// previously allocated range.
    unsafe fn purge(&self, _ptr: NonNull<u8>, _len: usize) {}
}

// ---------------------------------------------------------------------------
// MmapAllocator — production backend (unix)
// ---------------------------------------------------------------------------

// r[impl pool.mmap]
#[cfg(unix)]
pub struct MmapAllocator;

#[cfg(unix)]
unsafe impl PageAllocator for MmapAllocator {
    fn alloc(&self, layout: Layout) -> Option<NonNull<u8>> {
        let size = layout.size();
        let align = layout.align();

        // Over-allocate by `align` then trim leading/trailing excess.
        // Alternative: skip the munmap trims and waste up to `align` bytes
        // of virtual address space per allocation (~3% for 64KB/2MiB).
        let alloc_size = size + align;
        let raw = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                alloc_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return None;
        }

        let raw_addr = raw as usize;
        let aligned_addr = (raw_addr + align - 1) & !(align - 1);

        let leading = aligned_addr - raw_addr;
        if leading > 0 {
            unsafe { libc::munmap(raw, leading) };
        }

        let trailing = (raw_addr + alloc_size) - (aligned_addr + size);
        if trailing > 0 {
            unsafe { libc::munmap((aligned_addr + size) as *mut libc::c_void, trailing) };
        }

        NonNull::new(aligned_addr as *mut u8)
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { libc::munmap(ptr.as_ptr().cast(), layout.size()) };
    }

    // r[impl sys.purge-pages]
    unsafe fn purge(&self, ptr: NonNull<u8>, len: usize) {
        // SAFETY: caller guarantees ptr/len describe a page-aligned region
        // within a live mmap'd range.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_DONTNEED);
        }
        #[cfg(target_os = "macos")]
        unsafe {
            libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_FREE);
        }
    }
}

// ---------------------------------------------------------------------------
// SystemAllocator — test/Miri backend
// ---------------------------------------------------------------------------

/// Page allocator backed by the global Rust allocator.
///
/// Suitable for tests and Miri where mmap syscalls are unavailable.
/// Must not be used when clmalloc is itself the `#[global_allocator]`.
#[cfg_attr(not(test), expect(dead_code))]
pub struct SystemAllocator;

unsafe impl PageAllocator for SystemAllocator {
    fn alloc(&self, layout: Layout) -> Option<NonNull<u8>> {
        NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) };
    }
}
