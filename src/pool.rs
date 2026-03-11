//! Global page pool: manages OS memory and distributes slabs to heaps.
//!
//! All pool state (free slab list + segment carving) is protected by a
//! single `spin::Mutex`. This is a cold path — accessed only when a heap
//! needs a new slab or returns one — so lock-free complexity is unnecessary.

use core::alloc::Layout;
use core::mem::{align_of, size_of};
use core::ptr::{NonNull, null_mut};

use crate::slab::SLAB_SIZE;
use crate::sys::PageAllocator;

const SEGMENT_SIZE: usize = 2 * 1024 * 1024; // 2 MiB = 32 slabs
const MAX_SEGMENTS: usize = 4096;

#[repr(C, align(65536))]
struct SlabPage([u8; SLAB_SIZE]);
const _: () = assert!(size_of::<SlabPage>() == SLAB_SIZE);
const _: () = assert!(align_of::<SlabPage>() == SLAB_SIZE);

#[repr(C, align(65536))]
struct Segment([u8; SEGMENT_SIZE]);
const _: () = assert!(align_of::<Segment>() == SLAB_SIZE);

type Link = *mut SlabPage;

struct PoolState {
    free_head: Link,
    segment_cursor: Link,
    segment_end: Link,
    segments: [*mut Segment; MAX_SEGMENTS],
    segment_count: usize,
}

// SAFETY: PoolState is only accessed under the spin lock.
unsafe impl Send for PoolState {}

// r[impl pool.thread-safe] r[impl pool.alloc-slab] r[impl slab.alloc-from-pool]
pub struct PagePool<P: PageAllocator> {
    page_alloc: P,
    state: spin::Mutex<PoolState>,
}

unsafe impl<P: PageAllocator> Send for PagePool<P> {}
unsafe impl<P: PageAllocator> Sync for PagePool<P> {}

impl<P: PageAllocator> PagePool<P> {
    pub const fn new(page_alloc: P) -> Self {
        Self {
            page_alloc,
            state: spin::Mutex::new(PoolState {
                free_head: null_mut::<SlabPage>(),
                segment_cursor: null_mut::<SlabPage>(),
                segment_end: null_mut::<SlabPage>(),
                segments: [null_mut::<Segment>(); MAX_SEGMENTS],
                segment_count: 0,
            }),
        }
    }

    /// Allocate a `SLAB_SIZE`-aligned region of `SLAB_SIZE` bytes.
    ///
    /// Fast path: pop from the free list.
    /// Slow path: carve from the current segment.
    /// Slowest path: allocate a new segment from the page allocator.
    pub fn alloc_slab(&self) -> Option<NonNull<u8>> {
        let mut state = self.state.lock();

        if !state.free_head.is_null() {
            let slab = state.free_head;
            state.free_head = unsafe { slab.cast::<Link>().read() };
            return Some(unsafe { NonNull::new_unchecked(slab.cast()) });
        }

        // r[impl pool.batch-mmap]
        if state.segment_cursor >= state.segment_end {
            let base = self.page_alloc.alloc(Layout::new::<Segment>())?;
            let segment = base.as_ptr().cast::<Segment>();
            if state.segment_count >= MAX_SEGMENTS {
                unsafe { self.page_alloc.dealloc(base, Layout::new::<Segment>()) };
                return None;
            }
            let idx = state.segment_count;
            state.segments[idx] = segment;
            state.segment_count = idx + 1;
            state.segment_cursor = base.as_ptr().cast();
            state.segment_end = unsafe { base.as_ptr().add(SEGMENT_SIZE).cast() };
        }

        let slab = state.segment_cursor;
        state.segment_cursor = unsafe { slab.add(1) };
        Some(unsafe { NonNull::new_unchecked(slab.cast()) })
    }

    /// Return a fully-free slab to the pool for reuse.
    ///
    /// The first pointer-sized bytes of the slab's memory are used as the
    /// free list next-pointer (the slab is fully free, so its memory is unused).
    pub fn dealloc_slab(&self, base: NonNull<u8>) {
        let mut state = self.state.lock();
        let slab: Link = base.as_ptr().cast();
        unsafe { slab.cast::<Link>().write(state.free_head) };
        state.free_head = slab;
    }

    // r[impl pool.large-alloc]
    /// Allocate memory for a request exceeding the max size class.
    ///
    /// Delegates directly to the page allocator; no pooling.
    pub fn alloc_large(&self, layout: Layout) -> Option<NonNull<u8>> {
        self.page_alloc.alloc(layout)
    }

    // r[impl pool.large-dealloc]
    /// Deallocate a large allocation.
    ///
    /// # Safety
    /// `ptr` must have been returned by `alloc_large` with the same `layout`.
    pub unsafe fn dealloc_large(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { self.page_alloc.dealloc(ptr, layout) };
    }
}

impl<P: PageAllocator> Drop for PagePool<P> {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        for i in 0..state.segment_count {
            let segment = state.segments[i];
            if let Some(nn) = NonNull::new(segment) {
                unsafe { self.page_alloc.dealloc(nn.cast(), Layout::new::<Segment>()) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::SystemAllocator;
    use std::collections::HashSet;

    fn pool() -> PagePool<SystemAllocator> {
        PagePool::new(SystemAllocator)
    }

    // r[verify pool.alloc-slab] r[verify slab.alloc-from-pool]
    #[test]
    fn alloc_slab_aligned_and_unique() {
        let pool = pool();
        let mut slabs = Vec::new();
        for _ in 0..64 {
            let slab = pool.alloc_slab().unwrap();
            assert_eq!(slab.as_ptr() as usize % SLAB_SIZE, 0, "slab not aligned");
            slabs.push(slab);
        }
        let addrs: HashSet<usize> = slabs.iter().map(|s| s.as_ptr() as usize).collect();
        assert_eq!(addrs.len(), 64, "duplicate slab");
        for slab in slabs {
            pool.dealloc_slab(slab);
        }
    }

    // r[verify pool.batch-mmap]
    #[test]
    fn crosses_segment_boundary() {
        let pool = pool();
        let slabs_per_segment = SEGMENT_SIZE / SLAB_SIZE;
        let mut slabs = Vec::new();
        for _ in 0..(slabs_per_segment + 4) {
            slabs.push(pool.alloc_slab().unwrap());
        }
        let addrs: HashSet<usize> = slabs.iter().map(|s| s.as_ptr() as usize).collect();
        assert_eq!(addrs.len(), slabs_per_segment + 4);
        for slab in slabs {
            pool.dealloc_slab(slab);
        }
    }

    // r[verify pool.alloc-slab]
    #[test]
    fn dealloc_then_reuse() {
        let pool = pool();
        let s1 = pool.alloc_slab().unwrap();
        let addr = s1.as_ptr() as usize;
        pool.dealloc_slab(s1);
        let s2 = pool.alloc_slab().unwrap();
        assert_eq!(s2.as_ptr() as usize, addr, "expected reuse");
        pool.dealloc_slab(s2);
    }

    // r[verify pool.large-alloc] r[verify pool.large-dealloc]
    #[test]
    fn large_alloc_round_trip() {
        let pool = pool();
        let layout = Layout::from_size_align(1 << 20, 4096).unwrap();
        let ptr = pool.alloc_large(layout).unwrap();
        assert_eq!(ptr.as_ptr() as usize % 4096, 0);
        unsafe { pool.dealloc_large(ptr, layout) };
    }

    // r[verify pool.mmap]
    #[cfg(unix)]
    #[test]
    fn mmap_alloc_round_trip() {
        use crate::sys::MmapAllocator;
        let pool = PagePool::new(MmapAllocator);
        let slab = pool.alloc_slab().unwrap();
        assert_eq!(slab.as_ptr() as usize % SLAB_SIZE, 0);
        pool.dealloc_slab(slab);
    }

    // r[verify pool.thread-safe]
    #[test]
    fn concurrent_alloc_dealloc() {
        use std::sync::Arc;
        let pool = Arc::new(pool());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || {
                    let mut slabs = Vec::new();
                    for _ in 0..32 {
                        slabs.push(pool.alloc_slab().unwrap());
                    }
                    for slab in slabs {
                        pool.dealloc_slab(slab);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
}
