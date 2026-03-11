//! Thread-local heap: per-thread allocation state that wires together
//! size classes, slabs, and the global page pool.
//!
//! Each heap owns one active slab per size class. Allocation is contention-free
//! (no atomics on the fast path). Deallocation dispatches to local or remote
//! free lists based on slab ownership.

use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::pool::PagePool;
use crate::size_class::{self, NUM_CLASSES};
use crate::slab::{Slab, SlabBase, SlabRef};
use crate::sys::PageAllocator;

static NEXT_HEAP_ID: AtomicUsize = AtomicUsize::new(1);

fn next_heap_id() -> usize {
    NEXT_HEAP_ID.fetch_add(1, Ordering::Relaxed)
}

// r[impl heap.class-bins]
pub struct Heap<'pool, P: PageAllocator> {
    pool: &'pool PagePool<P>,
    // r[impl heap.identity]
    id: usize,
    bins: [Option<Slab>; NUM_CLASSES],
    /// Exhausted slabs kept for local dealloc. Chained via `next_abandoned`
    /// pointer in the slab header. Deallocs landing on these slabs use the
    /// local free list (no atomics). Promoted back to active when slots free up.
    retired_heads: [Option<NonNull<SlabBase>>; NUM_CLASSES],
}

impl<'pool, P: PageAllocator> Heap<'pool, P> {
    // r[impl heap.thread-local]
    pub fn new(pool: &'pool PagePool<P>) -> Self {
        const NONE: Option<Slab> = None;
        Self {
            pool,
            id: next_heap_id(),
            bins: [NONE; NUM_CLASSES],
            retired_heads: [None; NUM_CLASSES],
        }
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn id(&self) -> usize {
        self.id
    }

    // r[impl heap.alloc-fast-path] r[impl heap.slab-request]
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let idx = match size_class::class_index(layout) {
            Some(idx) => idx,
            None => return self.pool.alloc_large(layout),
        };

        if let Some(slab) = &mut self.bins[idx] {
            if let Some(ptr) = slab.alloc() {
                return Some(ptr);
            }
            slab.drain_remote();
            if let Some(ptr) = slab.alloc() {
                return Some(ptr);
            }
            self.retire_active(idx);
        }

        // Walk retired list: find a slab with free slots and promote it.
        self.promote_retired(idx);
        if self.bins[idx].is_some() {
            return self.alloc(layout);
        }

        self.request_slab(idx)
    }

    fn request_slab(&mut self, class_idx: usize) -> Option<NonNull<u8>> {
        if let Some(ptr) = self.try_adopt(class_idx) {
            return Some(ptr);
        }
        let raw = self.pool.alloc_slab()?;
        let mut slab = unsafe { Slab::init(raw, class_idx as u8, self.id) };
        let ptr = slab.alloc();
        self.bins[class_idx] = Some(slab);
        ptr
    }

    // r[impl heap.abandon]
    fn try_adopt(&mut self, class_idx: usize) -> Option<NonNull<u8>> {
        // Drain the abandoned list: return fully-free slabs to the pool,
        // adopt the first slab that has allocable slots.
        while let Some(mut slab) = self.pool.adopt_slab(class_idx) {
            slab.set_heap_id(self.id);
            slab.drain_remote();
            if slab.is_fully_free() {
                self.pool.dealloc_slab(slab.into_raw());
                continue;
            }
            if let Some(ptr) = slab.alloc() {
                self.bins[class_idx] = Some(slab);
                return Some(ptr);
            }
            // Still full after drain — put it back and stop scanning.
            // Remote frees haven't arrived yet; a fresh slab is needed.
            self.pool.abandon_slab(slab);
            break;
        }
        None
    }

    // r[impl heap.alloc-fast-path]
    /// # Safety
    /// - `ptr` must have been returned by `alloc` on some `Heap` sharing the
    ///   same `PagePool`.
    /// - `layout` must match the layout passed to the original `alloc` call.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let idx = match size_class::class_index(layout) {
            Some(idx) => idx,
            None => return unsafe { self.pool.dealloc_large(ptr, layout) },
        };

        let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };

        if slab_ref.heap_id() == self.id {
            if let Some(active) = &mut self.bins[idx]
                && active.as_ref().header_eq(&slab_ref)
            {
                active.dealloc_local(ptr);
                return;
            }
            // Walk the retired list for this class.
            let mut cursor = self.retired_heads[idx];
            while let Some(base) = cursor {
                let mut slab = unsafe { Slab::from_raw(base) };
                if slab.as_ref().header_eq(&slab_ref) {
                    slab.dealloc_local(ptr);
                    return;
                }
                cursor = slab.next_abandoned();
            }
        }

        slab_ref.dealloc_remote(ptr);
    }

    #[cfg(test)]
    fn drain_remote_all(&mut self) {
        for slab in self.bins.iter_mut().flatten() {
            slab.drain_remote();
        }
        for idx in 0..NUM_CLASSES {
            let mut cursor = self.retired_heads[idx];
            while let Some(base) = cursor {
                let mut slab = unsafe { Slab::from_raw(base) };
                slab.drain_remote();
                cursor = slab.next_abandoned();
            }
        }
    }

    /// Walk the retired list for `class_idx`. Drain remote on each slab:
    /// - Fully free → return to pool and unlink.
    /// - Has free slots → unlink and promote to active bin.
    /// - Still full → leave in list.
    fn promote_retired(&mut self, class_idx: usize) {
        let mut prev: *mut Option<NonNull<SlabBase>> = &mut self.retired_heads[class_idx];
        while let Some(base) = unsafe { *prev } {
            let mut slab = unsafe { Slab::from_raw(base) };
            let next = slab.next_abandoned();
            slab.drain_remote();
            if slab.is_fully_free() {
                unsafe { *prev = next };
                self.pool.dealloc_slab(slab.into_raw());
                continue;
            }
            if slab.free_count() > 0 {
                unsafe { *prev = next };
                slab.set_next_abandoned(None);
                self.bins[class_idx] = Some(slab);
                return;
            }
            prev = slab.next_abandoned_mut();
        }
    }

    /// Move the exhausted active slab to the retired linked list for its class.
    fn retire_active(&mut self, class_idx: usize) {
        if let Some(mut slab) = self.bins[class_idx].take() {
            slab.set_next_abandoned(self.retired_heads[class_idx]);
            self.retired_heads[class_idx] = Some(slab.into_raw());
        }
    }

    /// Retire the active slab during thread exit. Fully-free slabs go back
    /// to the pool; partially-used slabs are abandoned for adoption.
    fn retire_slab(&mut self, class_idx: usize) {
        if let Some(mut slab) = self.bins[class_idx].take() {
            slab.drain_remote();
            if slab.is_fully_free() {
                self.pool.dealloc_slab(slab.into_raw());
            } else {
                // r[impl heap.abandon]
                self.pool.abandon_slab(slab);
            }
        }
    }
}

// r[impl heap.thread-exit]
impl<P: PageAllocator> Drop for Heap<'_, P> {
    fn drop(&mut self) {
        for idx in 0..NUM_CLASSES {
            self.retire_slab(idx);
            // Drain the retired list: return free slabs to pool, abandon the rest.
            let mut cursor = self.retired_heads[idx].take();
            while let Some(base) = cursor {
                let mut slab = unsafe { Slab::from_raw(base) };
                cursor = slab.next_abandoned();
                slab.set_next_abandoned(None);
                slab.drain_remote();
                if slab.is_fully_free() {
                    self.pool.dealloc_slab(slab.into_raw());
                } else {
                    // r[impl heap.abandon]
                    self.pool.abandon_slab(slab);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::SystemAllocator;
    use std::sync::Arc;

    fn pool() -> PagePool<SystemAllocator> {
        PagePool::new(SystemAllocator)
    }

    // r[verify heap.thread-local] r[verify heap.identity]
    #[test]
    fn heap_ids_are_unique() {
        let pool = pool();
        let h1 = Heap::new(&pool);
        let h2 = Heap::new(&pool);
        assert_ne!(h1.id(), h2.id());
    }

    // r[verify heap.class-bins] r[verify heap.alloc-fast-path]
    #[test]
    fn alloc_dealloc_round_trip() {
        let pool = pool();
        let mut heap = Heap::new(&pool);

        for size in [8, 16, 64, 256, 1024, 4096, 32768] {
            let layout = Layout::from_size_align(size, 1).unwrap();
            let ptr = heap.alloc(layout).unwrap();
            unsafe { heap.dealloc(ptr, layout) };
        }
    }

    // r[verify heap.slab-request]
    #[test]
    fn exhaust_slab_gets_new_one() {
        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(8, 1).unwrap();

        let first = heap.alloc(layout).unwrap();
        let first_slab_ref = unsafe { SlabRef::from_interior_ptr(first.as_ptr()) };

        let slot_count = first_slab_ref.slot_count();
        let mut ptrs = vec![first];

        for _ in 1..slot_count {
            ptrs.push(heap.alloc(layout).unwrap());
        }

        let next = heap.alloc(layout).unwrap();
        let next_slab_ref = unsafe { SlabRef::from_interior_ptr(next.as_ptr()) };
        assert!(!first_slab_ref.header_eq(&next_slab_ref), "should be a different slab");

        unsafe { heap.dealloc(next, layout) };
        for ptr in ptrs {
            unsafe { heap.dealloc(ptr, layout) };
        }
    }

    // r[verify heap.alloc-fast-path]
    #[test]
    fn drain_remote_reuses_slots() {
        let pool = Arc::new(pool());
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();

        let ptr = heap.alloc(layout).unwrap();
        let raw = ptr.as_ptr() as usize;

        let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
        let slot_count = slab_ref.slot_count();

        let mut others = Vec::new();
        for _ in 1..slot_count {
            others.push(heap.alloc(layout).unwrap());
        }

        std::thread::spawn(move || {
            let ptr = NonNull::new(raw as *mut u8).unwrap();
            let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
            slab_ref.dealloc_remote(ptr);
        })
        .join()
        .unwrap();

        let recovered = heap.alloc(layout).unwrap();
        assert_eq!(recovered.as_ptr() as usize, raw);

        unsafe { heap.dealloc(recovered, layout) };
        for ptr in others {
            unsafe { heap.dealloc(ptr, layout) };
        }
    }

    // r[verify heap.alloc-fast-path]
    #[test]
    fn large_allocation_passthrough() {
        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(1 << 20, 4096).unwrap();
        let ptr = heap.alloc(layout).unwrap();
        assert_eq!(ptr.as_ptr() as usize % 4096, 0);
        unsafe { heap.dealloc(ptr, layout) };
    }

    // r[verify heap.thread-exit]
    #[test]
    fn drop_returns_free_slabs_to_pool() {
        let pool = pool();
        let slab_before = pool.alloc_slab().unwrap();
        pool.dealloc_slab(slab_before);

        {
            let mut heap = Heap::new(&pool);
            let layout = Layout::from_size_align(64, 1).unwrap();
            let ptr = heap.alloc(layout).unwrap();
            unsafe { heap.dealloc(ptr, layout) };
        }

        // After heap drop, the slab should be back in the pool.
        // Allocating again should succeed (reuses the returned slab).
        let reused = pool.alloc_slab().unwrap();
        pool.dealloc_slab(reused);
    }

    // r[verify heap.identity]
    #[test]
    fn cross_thread_dealloc_uses_remote() {
        let pool = Arc::new(pool());
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();

        let ptr = heap.alloc(layout).unwrap();
        let raw = ptr.as_ptr() as usize;
        let pool2 = Arc::clone(&pool);

        std::thread::spawn(move || {
            let mut other_heap = Heap::new(&pool2);
            let ptr = NonNull::new(raw as *mut u8).unwrap();
            unsafe { other_heap.dealloc(ptr, layout) };
        })
        .join()
        .unwrap();

        heap.drain_remote_all();

        let recovered = heap.alloc(layout).unwrap();
        assert_eq!(recovered.as_ptr() as usize, raw);
        unsafe { heap.dealloc(recovered, layout) };
    }

    // r[verify heap.abandon]
    #[test]
    fn abandoned_slab_is_adopted_by_new_heap() {
        let pool = pool();
        let layout = Layout::from_size_align(64, 1).unwrap();

        let outstanding_ptr;
        {
            let mut heap1 = Heap::new(&pool);
            outstanding_ptr = heap1.alloc(layout).unwrap();
            // heap1 drops here — slab has 1 outstanding alloc, gets abandoned
        }

        let mut heap2 = Heap::new(&pool);
        // heap2's first alloc for this class should adopt the abandoned slab
        let ptr2 = heap2.alloc(layout).unwrap();
        let slab_ref1 = unsafe { SlabRef::from_interior_ptr(outstanding_ptr.as_ptr()) };
        let slab_ref2 = unsafe { SlabRef::from_interior_ptr(ptr2.as_ptr()) };
        assert!(slab_ref1.header_eq(&slab_ref2), "heap2 should adopt heap1's slab");

        // heap_id should now be heap2's
        assert_eq!(slab_ref2.heap_id(), heap2.id());

        unsafe { heap2.dealloc(outstanding_ptr, layout) };
        unsafe { heap2.dealloc(ptr2, layout) };
    }

    // r[verify heap.abandon]
    #[test]
    fn abandoned_slab_with_remote_frees_after_abandon() {
        let pool = Arc::new(pool());
        let layout = Layout::from_size_align(64, 1).unwrap();

        let raw_ptr;
        {
            let mut heap1 = Heap::new(&pool);
            let ptr = heap1.alloc(layout).unwrap();
            raw_ptr = ptr.as_ptr() as usize;
            // heap1 drops — slab abandoned with 1 outstanding slot
        }

        // Simulate a remote free arriving after abandonment
        let slab_ref = unsafe { SlabRef::from_interior_ptr(raw_ptr as *const u8) };
        slab_ref.dealloc_remote(NonNull::new(raw_ptr as *mut u8).unwrap());

        // heap2 adopts — after drain_remote the slab should be fully free
        // and returned to the pool, so heap2 gets a fresh slab instead
        let mut heap2 = Heap::new(&pool);
        let ptr2 = heap2.alloc(layout).unwrap();
        let slab_ref2 = unsafe { SlabRef::from_interior_ptr(ptr2.as_ptr()) };
        assert_eq!(slab_ref2.heap_id(), heap2.id());

        unsafe { heap2.dealloc(ptr2, layout) };
    }

    // r[verify heap.abandon]
    #[test]
    fn cross_thread_abandon_adopt() {
        let pool = Arc::new(pool());
        let layout = Layout::from_size_align(256, 1).unwrap();
        let pool2 = Arc::clone(&pool);

        let raw_ptrs: Vec<usize> = std::thread::spawn(move || {
            let mut heap = Heap::new(&pool2);
            let mut ptrs = Vec::new();
            for _ in 0..10 {
                ptrs.push(heap.alloc(layout).unwrap().as_ptr() as usize);
            }
            // heap drops — slab abandoned with 10 outstanding allocs
            ptrs
        })
        .join()
        .unwrap();

        let mut heap2 = Heap::new(&pool);
        // heap2 should adopt the abandoned slab
        let new_ptr = heap2.alloc(layout).unwrap();
        let new_slab_ref = unsafe { SlabRef::from_interior_ptr(new_ptr.as_ptr()) };
        let old_slab_ref = unsafe { SlabRef::from_interior_ptr(raw_ptrs[0] as *const u8) };
        assert!(old_slab_ref.header_eq(&new_slab_ref), "should adopt the abandoned slab");

        // Free everything
        for raw in &raw_ptrs {
            unsafe { heap2.dealloc(NonNull::new(*raw as *mut u8).unwrap(), layout) };
        }
        unsafe { heap2.dealloc(new_ptr, layout) };
    }
}
