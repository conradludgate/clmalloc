//! Global page pool: manages OS memory and distributes slabs to heaps.
//!
//! All pool state (free slab list + segment carving) is protected by a
//! single `spin::Mutex`. This is a cold path — accessed only when a heap
//! needs a new slab or returns one — so lock-free complexity is unnecessary.
//!
//! The pool releases physical pages back to the OS when an entire segment's
//! slabs are returned (purge). Segment allocation (mmap) is performed
//! outside the lock to avoid blocking other threads on syscall latency.

use core::alloc::Layout;
use core::mem::{align_of, size_of};
use core::ptr::{NonNull, null_mut};

use crate::size_class::NUM_CLASSES;
use crate::slab::{SLAB_SIZE, Slab, SlabBase, SlabList, slab_list_pop, slab_list_push};
use crate::sys::PageAllocator;

const SEGMENT_SIZE: usize = 2 * 1024 * 1024; // 2 MiB
const MAX_SEGMENTS: usize = 4096;
const SLABS_PER_SEGMENT: usize = SEGMENT_SIZE / SLAB_SIZE;

#[repr(C, align(65536))]
struct SlabPage([u8; SLAB_SIZE]);
const _: () = assert!(size_of::<SlabPage>() == SLAB_SIZE);
const _: () = assert!(align_of::<SlabPage>() == SLAB_SIZE);

#[repr(C, align(65536))]
struct Segment([u8; SEGMENT_SIZE]);
const _: () = assert!(align_of::<Segment>() == SLAB_SIZE);

type Link = *mut SlabPage;

/// Free list of slab pages. Each free page stores a next-pointer in its
/// first pointer-sized bytes (always aligned since SlabPage is 64KB-aligned).
struct PageFreeList {
    head: Link,
}

impl PageFreeList {
    const EMPTY: Self = Self { head: null_mut() };

    fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    fn push(&mut self, slab: Link) {
        // SAFETY: slab is a valid SLAB_SIZE-aligned page; first word holds the link.
        unsafe { slab.cast::<Link>().write(self.head) };
        self.head = slab;
    }

    fn pop(&mut self) -> Option<NonNull<SlabPage>> {
        let slab = NonNull::new(self.head)?;
        // SAFETY: slab is from our free list; first word holds the next link.
        self.head = unsafe { slab.as_ptr().cast::<Link>().read() };
        Some(slab)
    }

    /// Remove all nodes where `pred` returns true.
    fn remove_if(&mut self, mut pred: impl FnMut(Link) -> bool) {
        let mut prev: *mut Link = &raw mut self.head;
        // SAFETY: prev points at head or at a slab's link field; all are valid.
        unsafe {
            while !(*prev).is_null() {
                let slab = *prev;
                if pred(slab) {
                    *prev = slab.cast::<Link>().read();
                } else {
                    prev = slab.cast::<Link>();
                }
            }
        }
    }
}

struct PoolState {
    free_list: PageFreeList,
    abandoned_heads: [SlabList; NUM_CLASSES],
    segment_cursor: Link,
    segment_end: Link,
    /// Index into `segments[]` for the segment currently being carved.
    carving_idx: usize,
    segments: [*mut Segment; MAX_SEGMENTS],
    segment_count: usize,
    /// Number of slabs from each segment currently in use (allocated to a
    /// heap or on the abandoned list — i.e. not on the free list and not
    /// still uncarved).
    seg_outstanding: [u8; MAX_SEGMENTS],
    /// Slabs currently handed out (not on the free list or uncarved).
    #[cfg(feature = "metrics")]
    outstanding_slabs: u64,
    /// Bytes currently mmap'd for large allocations (bypassing the slab system).
    #[cfg(feature = "metrics")]
    large_mapped_bytes: u64,
    #[cfg(feature = "metrics")]
    metrics: crate::metrics::PoolMetrics,
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
                free_list: PageFreeList::EMPTY,
                abandoned_heads: [None; NUM_CLASSES],
                segment_cursor: null_mut::<SlabPage>(),
                segment_end: null_mut::<SlabPage>(),
                carving_idx: 0,
                segments: [null_mut::<Segment>(); MAX_SEGMENTS],
                segment_count: 0,
                seg_outstanding: [0; MAX_SEGMENTS],
                #[cfg(feature = "metrics")]
                outstanding_slabs: 0,
                #[cfg(feature = "metrics")]
                large_mapped_bytes: 0,
                #[cfg(feature = "metrics")]
                metrics: crate::metrics::PoolMetrics::new(),
            }),
        }
    }

    /// Allocate a `SLAB_SIZE`-aligned region of `SLAB_SIZE` bytes.
    ///
    /// Fast path: pop from the free list.
    /// Slow path: carve from the current segment.
    /// Slowest path: allocate a new segment from the page allocator (outside
    /// the lock).
    #[cold]
    pub fn alloc_slab(&self) -> Option<NonNull<SlabBase>> {
        let mut state = self.state.lock();
        #[cfg(feature = "metrics")]
        {
            state.metrics.pool_lock_count += 1;
        }

        if let Some(slab) = state.free_list.pop() {
            let seg = Self::find_segment(&state, slab.as_ptr() as usize);
            state.seg_outstanding[seg] += 1;
            #[cfg(feature = "metrics")]
            {
                state.outstanding_slabs += 1;
            }
            return Some(slab.cast());
        }

        if state.segment_cursor < state.segment_end {
            let slab = state.segment_cursor;
            // SAFETY: segment_cursor is within [segment_end - SLABS_PER_SEGMENT, segment_end).
            state.segment_cursor = unsafe { slab.add(1) };
            let seg = state.carving_idx;
            state.seg_outstanding[seg] += 1;
            #[cfg(feature = "metrics")]
            {
                state.outstanding_slabs += 1;
            }
            // SAFETY: slab is non-null (segment_cursor < segment_end).
            return Some(unsafe { NonNull::new_unchecked(slab.cast()) });
        }

        // r[impl pool.no-syscall-under-lock] r[impl pool.batch-mmap]
        drop(state);
        self.alloc_slab_slow()
    }

    #[cold]
    #[inline(never)]
    fn alloc_slab_slow(&self) -> Option<NonNull<SlabBase>> {
        let base = self.page_alloc.alloc(Layout::new::<Segment>())?;
        let mut state = self.state.lock();
        #[cfg(feature = "metrics")]
        {
            state.metrics.pool_lock_count += 1;
            state.metrics.segment_mmap_count += 1;
        }

        // r[impl pool.no-panic-under-lock]
        if state.segment_count >= MAX_SEGMENTS {
            drop(state);
            // SAFETY: base was just allocated with Layout::new::<Segment>().
            unsafe { self.page_alloc.dealloc(base, Layout::new::<Segment>()) };
            return None;
        }

        let seg_idx = state.segment_count;
        state.segments[seg_idx] = base.as_ptr().cast();
        state.segment_count = seg_idx + 1;
        state.seg_outstanding[seg_idx] = 1;
        #[cfg(feature = "metrics")]
        {
            state.outstanding_slabs += 1;
        }

        if state.segment_cursor < state.segment_end {
            // Another thread set up a carving segment while we were in mmap.
            // Push remaining slabs to the free list so they aren't lost.
            let seg_base: Link = base.as_ptr().cast();
            for i in (1..SLABS_PER_SEGMENT).rev() {
                // SAFETY: i in 1..SLABS_PER_SEGMENT, segment has SLABS_PER_SEGMENT slabs.
                let slab = unsafe { seg_base.add(i) };
                state.free_list.push(slab);
            }
        } else {
            state.carving_idx = seg_idx;
            // SAFETY: base is page-aligned from mmap; SlabPage has SLAB_SIZE alignment.
            state.segment_cursor = unsafe { base.as_ptr().cast::<SlabPage>().add(1) };
            // SAFETY: base + SEGMENT_SIZE is within the allocated segment.
            state.segment_end = unsafe { base.as_ptr().add(SEGMENT_SIZE).cast() };
        }

        // SAFETY: base is non-null (from page_alloc.alloc).
        Some(unsafe { NonNull::new_unchecked(base.as_ptr().cast()) })
    }

    /// Return a fully-free slab to the pool for reuse.
    ///
    /// When the last outstanding slab of a segment is returned, the entire
    /// segment is released back to the OS via munmap. Otherwise, the slab's
    /// physical pages are released via `madvise` so the OS can reclaim them
    /// while keeping the virtual address range available for reuse.
    // r[impl pool.purge] r[impl pool.purge-free-slab]
    #[cold]
    pub fn dealloc_slab(&self, base: NonNull<SlabBase>) {
        let slab: Link = base.as_ptr().cast();
        let mut state = self.state.lock();
        #[cfg(feature = "metrics")]
        {
            state.metrics.pool_lock_count += 1;
        }
        state.free_list.push(slab);

        let seg = Self::find_segment(&state, slab as usize);
        state.seg_outstanding[seg] -= 1;
        #[cfg(feature = "metrics")]
        {
            state.outstanding_slabs -= 1;
        }

        if state.seg_outstanding[seg] == 0 && Self::segment_fully_carved(&state, seg) {
            let segment_ptr = state.segments[seg];
            Self::remove_segment_slabs(&mut state, segment_ptr as usize);
            Self::swap_remove_segment(&mut state, seg);
            #[cfg(feature = "metrics")]
            {
                state.metrics.segment_munmap_count += 1;
            }
            drop(state);
            // SAFETY: segment_ptr was obtained from mmap and not yet unmapped; layout matches.
            unsafe {
                self.page_alloc.dealloc(
                    NonNull::new_unchecked(segment_ptr.cast()),
                    Layout::new::<Segment>(),
                );
            }
        } else {
            #[cfg(feature = "metrics")]
            {
                state.metrics.slab_purge_count += 1;
            }
            drop(state);
            // SAFETY: base points to a SLAB_SIZE-aligned, SLAB_SIZE-byte region
            // within a live mmap'd segment.
            unsafe { self.page_alloc.purge(base.cast(), SLAB_SIZE) };
        }
    }

    /// True if all slabs in the segment have been carved (none still in
    /// the uncarved cursor region).
    fn segment_fully_carved(state: &PoolState, seg_idx: usize) -> bool {
        let seg_base = state.segments[seg_idx] as usize;
        let seg_end = seg_base + SEGMENT_SIZE;
        let cursor = state.segment_cursor as usize;
        // If the cursor falls within this segment, uncarved slabs remain.
        !(cursor >= seg_base && cursor < seg_end)
    }

    fn find_segment(state: &PoolState, addr: usize) -> usize {
        for i in 0..state.segment_count {
            let base = state.segments[i] as usize;
            if addr >= base && addr < base + SEGMENT_SIZE {
                return i;
            }
        }
        unreachable!("slab does not belong to any segment")
    }

    /// Walk the free list and unlink all slabs belonging to the given segment.
    fn remove_segment_slabs(state: &mut PoolState, seg_base: usize) {
        let seg_end = seg_base + SEGMENT_SIZE;
        state.free_list.remove_if(|slab| {
            let addr = slab as usize;
            addr >= seg_base && addr < seg_end
        });
    }

    /// Swap-remove a segment from all tracking arrays.
    fn swap_remove_segment(state: &mut PoolState, seg_idx: usize) {
        let last = state.segment_count - 1;
        if seg_idx != last {
            state.segments[seg_idx] = state.segments[last];
            state.seg_outstanding[seg_idx] = state.seg_outstanding[last];
            if state.carving_idx == last {
                state.carving_idx = seg_idx;
            }
        }
        state.segments[last] = null_mut();
        state.seg_outstanding[last] = 0;
        state.segment_count = last;
    }

    #[cfg(test)]
    fn segment_count(&self) -> usize {
        self.state.lock().segment_count
    }

    /// Place a non-fully-free slab on the abandoned list for its size class.
    ///
    /// Called during thread exit when a slab still has outstanding allocations.
    /// Another heap can later adopt it via `adopt_slab`.
    #[cold]
    pub fn abandon_slab(&self, slab: Slab) {
        let class_idx = slab.size_class_index();
        let mut state = self.state.lock();
        #[cfg(feature = "metrics")]
        {
            state.metrics.pool_lock_count += 1;
            state.metrics.abandon_count[class_idx] += 1;
        }
        slab_list_push(&mut state.abandoned_heads[class_idx], slab);
    }

    /// Try to adopt an abandoned slab for the given size class.
    ///
    /// Returns the slab with ownership transferred to the caller.
    /// The caller must `drain_remote` and `set_heap_id` before use.
    #[cold]
    pub fn adopt_slab(&self, class_idx: usize) -> Option<Slab> {
        let mut state = self.state.lock();
        #[cfg(feature = "metrics")]
        {
            state.metrics.pool_lock_count += 1;
        }
        if state.abandoned_heads[class_idx].is_none() {
            return None;
        }
        #[cfg(feature = "metrics")]
        {
            state.metrics.adopt_count[class_idx] += 1;
        }
        slab_list_pop(&mut state.abandoned_heads[class_idx])
    }

    // r[impl pool.large-alloc]
    /// Allocate memory for a request exceeding the max size class.
    ///
    /// Delegates directly to the page allocator; no pooling.
    #[cold]
    pub fn alloc_large(&self, layout: Layout) -> Option<NonNull<u8>> {
        let ptr = self.page_alloc.alloc(layout)?;
        #[cfg(feature = "metrics")]
        {
            self.state.lock().large_mapped_bytes += layout.size() as u64;
        }
        Some(ptr)
    }

    // r[impl pool.large-dealloc]
    /// Deallocate a large allocation.
    ///
    /// # Safety
    /// `ptr` must have been returned by `alloc_large` with the same `layout`.
    #[cold]
    pub unsafe fn dealloc_large(&self, ptr: NonNull<u8>, layout: Layout) {
        #[cfg(feature = "metrics")]
        {
            self.state.lock().large_mapped_bytes -= layout.size() as u64;
        }
        // SAFETY: caller guarantees ptr came from alloc_large with this layout.
        unsafe { self.page_alloc.dealloc(ptr, layout) };
    }
}

#[cfg(feature = "metrics")]
impl<P: PageAllocator> PagePool<P> {
    pub fn register_heap(&self, ptr: *const crate::metrics::HeapMetrics) {
        self.state.lock().metrics.register_heap(ptr);
    }

    pub fn deregister_heap(&self, ptr: *const crate::metrics::HeapMetrics) {
        self.state.lock().metrics.deregister_heap(ptr);
    }

    // r[impl metrics.global-snapshot] r[impl metrics.global-mapped]
    // r[impl metrics.global-active]
    pub fn snapshot(&self) -> crate::metrics::MetricsSnapshot {
        let mut snap = crate::metrics::MetricsSnapshot::new();
        let state = self.state.lock();
        snap.mapped = state.segment_count as u64 * SEGMENT_SIZE as u64
            + state.large_mapped_bytes;
        let outstanding_slabs = state.outstanding_slabs;
        state.metrics.aggregate_heap_metrics(&mut snap);
        drop(state);
        snap.finalize();
        let large_live = snap.large_alloc_bytes.saturating_sub(snap.large_dealloc_bytes);
        snap.active = outstanding_slabs * SLAB_SIZE as u64 + large_live;
        snap
    }
}

impl<P: PageAllocator> Drop for PagePool<P> {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        for i in 0..state.segment_count {
            let segment = state.segments[i];
            if let Some(nn) = NonNull::new(segment) {
                // SAFETY: segment was allocated with Layout::new::<Segment>(); we own it.
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

    // r[verify pool.purge]
    #[test]
    fn fully_free_segment_is_purged() {
        let pool = pool();
        let slabs_per_seg = SEGMENT_SIZE / SLAB_SIZE;

        let mut slabs = Vec::new();
        for _ in 0..slabs_per_seg {
            slabs.push(pool.alloc_slab().unwrap());
        }
        assert_eq!(pool.segment_count(), 1);

        for slab in slabs {
            pool.dealloc_slab(slab);
        }
        assert_eq!(pool.segment_count(), 0, "segment should be purged");

        // Pool is still usable: next alloc triggers a fresh mmap.
        let fresh = pool.alloc_slab().unwrap();
        assert_eq!(pool.segment_count(), 1);
        pool.dealloc_slab(fresh);
    }

    // r[verify pool.purge]
    #[test]
    fn partial_segment_not_purged() {
        let pool = pool();
        let slabs_per_seg = SEGMENT_SIZE / SLAB_SIZE;

        let mut slabs = Vec::new();
        for _ in 0..slabs_per_seg {
            slabs.push(pool.alloc_slab().unwrap());
        }

        // Return all but one — segment must NOT be purged.
        let kept = slabs.pop().unwrap();
        for slab in slabs {
            pool.dealloc_slab(slab);
        }
        assert_eq!(pool.segment_count(), 1, "segment should still exist");

        pool.dealloc_slab(kept);
        assert_eq!(pool.segment_count(), 0, "now it should be purged");
    }

    // r[verify pool.purge]
    #[test]
    fn uncarved_segment_not_purged() {
        let pool = pool();
        // Allocate just 1 slab from a segment (31 remain uncarved).
        let s = pool.alloc_slab().unwrap();
        pool.dealloc_slab(s);
        // Segment has uncarved slabs → must not be purged.
        assert_eq!(pool.segment_count(), 1);
    }

    // r[verify pool.no-syscall-under-lock]
    // Exercises the race path where multiple threads mmap concurrently.
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
