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
use crate::slab::{Slab, SlabRef};
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
}

impl<'pool, P: PageAllocator> Heap<'pool, P> {
    // r[impl heap.thread-local]
    pub fn new(pool: &'pool PagePool<P>) -> Self {
        const NONE: Option<Slab> = None;
        Self {
            pool,
            id: next_heap_id(),
            bins: [NONE; NUM_CLASSES],
        }
    }

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
            self.retire_slab(idx);
        }

        self.request_slab(idx)
    }

    fn request_slab(&mut self, class_idx: usize) -> Option<NonNull<u8>> {
        let raw = self.pool.alloc_slab()?;
        let mut slab = unsafe { Slab::init(raw, class_idx as u8, self.id) };
        let ptr = slab.alloc();
        self.bins[class_idx] = Some(slab);
        ptr
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

        if slab_ref.heap_id() == self.id
            && let Some(active) = &mut self.bins[idx]
            && active.as_ref().header_eq(&slab_ref)
        {
            active.dealloc_local(ptr);
            return;
        }

        slab_ref.dealloc_remote(ptr);
    }

    #[cfg(test)]
    fn drain_remote_all(&mut self) {
        for slab in self.bins.iter_mut().flatten() {
            slab.drain_remote();
        }
    }

    fn retire_slab(&mut self, class_idx: usize) {
        if let Some(mut slab) = self.bins[class_idx].take() {
            slab.drain_remote();
            if slab.is_fully_free() {
                self.pool.dealloc_slab(slab.into_raw());
            }
            // v1: non-fully-free retired slabs are dropped. The backing memory
            // stays valid (pool segments are never unmapped), and remote frees
            // continue to work via SlabRef. Slots on the remote list are leaked.
        }
    }
}

// r[impl heap.thread-exit]
impl<P: PageAllocator> Drop for Heap<'_, P> {
    fn drop(&mut self) {
        for idx in 0..NUM_CLASSES {
            self.retire_slab(idx);
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
}
