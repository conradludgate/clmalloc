//! Thread-local heap: per-thread allocation state that wires together
//! size classes, slabs, and the global page pool.
//!
//! Each heap owns one active slab per size class. Allocation is contention-free
//! (no atomics on the fast path). Deallocation dispatches to local or remote
//! free lists based on slab ownership.
//!
//! Non-active slabs are tracked in two per-class queues (mimalloc-style
//! page queues): a *full* queue for exhausted slabs and a *partial* queue
//! for slabs with known free slots. When the active slab is exhausted,
//! the heap pops from the partial queue in O(1). If the partial queue is
//! empty, the heap scans the full queue in one pass, draining remote frees
//! and collecting all newly-partial slabs. This amortises the scan cost.
//!
//! A per-size-class free cache (inspired by jemalloc's tcache) absorbs
//! alloc/free pairs without touching shared state. Non-active-slab frees
//! are pushed into the cache (no atomics); allocs pop from the cache first.
//! The cache is flushed in batch on overflow or thread exit, amortising
//! the cost of atomic CAS operations across many frees.

use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::pool::PagePool;
use crate::size_class::{self, NUM_CLASSES};
use crate::slab::{
    self, SLAB_MASK, Slab, SlabBase, SlabList, SlabListCursor, SlabRef, slab_list_pop,
    slab_list_push,
};
use crate::sys::PageAllocator;

static NEXT_HEAP_ID: AtomicUsize = AtomicUsize::new(1);

fn next_heap_id() -> usize {
    NEXT_HEAP_ID.fetch_add(1, Ordering::Relaxed)
}

// -- Free cache (tcache) -----------------------------------------------------

const CACHE_CAP: usize = 64;

// r[impl heap.free-cache]
struct FreeCache {
    entries: [NonNull<u8>; CACHE_CAP],
    count: u8,
}

impl FreeCache {
    const EMPTY: Self = Self {
        entries: [NonNull::dangling(); CACHE_CAP],
        count: 0,
    };

    #[inline]
    fn pop(&mut self) -> Option<NonNull<u8>> {
        if self.count == 0 {
            return None;
        }
        self.count -= 1;
        Some(self.entries[self.count as usize])
    }

    #[inline]
    fn push(&mut self, ptr: NonNull<u8>) {
        debug_assert!((self.count as usize) < CACHE_CAP);
        self.entries[self.count as usize] = ptr;
        self.count += 1;
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.count as usize >= CACHE_CAP
    }
}

// -- Heap --------------------------------------------------------------------

// r[impl heap.class-bins]
pub struct Heap<'pool, P: PageAllocator> {
    pool: &'pool PagePool<P>,
    // r[impl heap.identity]
    id: usize,
    bins: [Option<Slab>; NUM_CLASSES],
    // r[impl heap.page-queue]
    full_heads: [SlabList; NUM_CLASSES],
    partial_heads: [SlabList; NUM_CLASSES],
    caches: [FreeCache; NUM_CLASSES],
    #[cfg(feature = "metrics")]
    pub(crate) metrics: core::cell::UnsafeCell<crate::metrics::HeapMetrics>,
    // r[impl pprof.feature-gate]
    #[cfg(feature = "pprof")]
    sampler: crate::pprof::Sampler,
}

impl<'pool, P: PageAllocator> Heap<'pool, P> {
    // r[impl heap.thread-local]
    pub fn new(pool: &'pool PagePool<P>) -> Self {
        const NONE: Option<Slab> = None;
        let id = next_heap_id();
        Self {
            pool,
            id,
            bins: [NONE; NUM_CLASSES],
            full_heads: [None; NUM_CLASSES],
            partial_heads: [None; NUM_CLASSES],
            caches: [FreeCache::EMPTY; NUM_CLASSES],
            #[cfg(feature = "metrics")]
            metrics: core::cell::UnsafeCell::new(crate::metrics::HeapMetrics::ZERO),
            #[cfg(feature = "pprof")]
            sampler: crate::pprof::Sampler::new(id as u64),
        }
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn id(&self) -> usize {
        self.id
    }

    #[cfg_attr(not(feature = "metrics"), expect(dead_code))]
    pub fn pool(&self) -> &'pool PagePool<P> {
        self.pool
    }

    /// Shared reference to the metrics counters. The `UnsafeCell` wrapper
    /// prevents `&mut Heap` from invalidating raw pointers stored in the
    /// pool's heap registry (Stacked Borrows). Interior mutability is
    /// provided by `AtomicU64` fields.
    #[cfg(feature = "metrics")]
    #[inline(always)]
    pub(crate) fn metrics(&self) -> &crate::metrics::HeapMetrics {
        // SAFETY: UnsafeCell allows shared access; we hold no &mut Heap.
        unsafe { &*self.metrics.get() }
    }

    #[cfg(feature = "pprof")]
    #[inline(always)]
    fn maybe_sample(&mut self, ptr: NonNull<u8>, size: usize, class_idx: u8) {
        if crate::pprof::should_sample() && self.sampler.check(size) {
            crate::pprof::record_sample(ptr.as_ptr(), size, class_idx);
        }
    }

    // r[impl heap.alloc-fast-path] r[impl heap.slab-request] r[impl heap.free-cache]
    // r[impl heap.page-queue]
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let Some(idx) = size_class::class_index(layout) else {
            let ptr = self.pool.alloc_large(layout)?;
            #[cfg(feature = "metrics")]
            self.metrics().on_large_alloc(layout.size());
            #[cfg(feature = "pprof")]
            self.maybe_sample(ptr, layout.size(), u8::MAX);
            return Some(ptr);
        };

        let class_size = size_class::class_size(idx);

        if let Some(ptr) = self.caches[idx].pop() {
            #[cfg(feature = "metrics")]
            {
                self.metrics().on_alloc(idx, class_size);
                self.metrics().on_cache_hit(idx);
            }
            #[cfg(feature = "pprof")]
            self.maybe_sample(ptr, class_size, idx as u8);
            return Some(ptr);
        }

        if let Some(slab) = &mut self.bins[idx] {
            if let Some(ptr) = slab.alloc() {
                #[cfg(feature = "metrics")]
                self.metrics().on_alloc(idx, class_size);
                #[cfg(feature = "pprof")]
                self.maybe_sample(ptr, class_size, idx as u8);
                return Some(ptr);
            }
            slab.drain_remote();
            if let Some(ptr) = slab.alloc() {
                #[cfg(feature = "metrics")]
                self.metrics().on_alloc(idx, class_size);
                #[cfg(feature = "pprof")]
                self.maybe_sample(ptr, class_size, idx as u8);
                return Some(ptr);
            }
            self.retire_active(idx);
        }

        self.flush_cache(idx);

        if self.try_partial(idx) {
            return self.alloc(layout);
        }

        self.scan_full_list(idx);
        if self.bins[idx].is_some() {
            return self.alloc(layout);
        }

        if self.try_partial(idx) {
            return self.alloc(layout);
        }

        self.request_slab(idx, class_size)
    }

    #[allow(clippy::cast_possible_truncation)]
    #[cold]
    #[inline(never)]
    fn request_slab(
        &mut self,
        class_idx: usize,
        #[cfg_attr(not(any(feature = "metrics", feature = "pprof")), expect(unused))]
        class_size: usize,
    ) -> Option<NonNull<u8>> {
        if let Some(ptr) = self.try_adopt(class_idx) {
            #[cfg(feature = "metrics")]
            self.metrics().on_alloc(class_idx, class_size);
            #[cfg(feature = "pprof")]
            self.maybe_sample(ptr, class_size, class_idx as u8);
            return Some(ptr);
        }
        let raw = self.pool.alloc_slab()?;
        // SAFETY: raw is a valid, exclusively-owned slab page from the pool.
        let mut slab = unsafe { Slab::init(raw, class_idx as u8, self.id) };
        let ptr = slab.alloc();
        self.bins[class_idx] = Some(slab);
        #[cfg(feature = "metrics")]
        if ptr.is_some() {
            self.metrics().on_alloc(class_idx, class_size);
        }
        #[cfg(feature = "pprof")]
        if let Some(p) = ptr {
            self.maybe_sample(p, class_size, class_idx as u8);
        }
        ptr
    }

    #[cold]
    #[inline(never)]
    // r[impl heap.abandon] r[impl heap.eager-adopt]
    fn try_adopt(&mut self, class_idx: usize) -> Option<NonNull<u8>> {
        // Eagerly adopt ALL abandoned slabs for this class. This ensures
        // that frees to any of them are local (heap_id matches), avoiding
        // atomic CAS on the remote free list. Critical for workloads like
        // Larson where a successor thread inherits its predecessor's
        // allocations after the predecessor has exited.
        let mut result: Option<NonNull<u8>> = None;
        while let Some(mut slab) = self.pool.adopt_slab(class_idx) {
            slab.set_heap_id(self.id);
            slab.drain_remote();
            if slab.is_fully_free() {
                self.pool.dealloc_slab(slab.into_raw());
                continue;
            }
            if result.is_none()
                && let Some(ptr) = slab.alloc()
            {
                self.bins[class_idx] = Some(slab);
                result = Some(ptr);
                continue;
            }
            // Partial or full — put on the appropriate list so frees
            // to this slab are local (heap_id is ours).
            if slab.free_count() > 0 {
                slab_list_push(&mut self.partial_heads[class_idx], slab);
            } else {
                slab_list_push(&mut self.full_heads[class_idx], slab);
            }
        }
        result
    }

    // r[impl heap.dealloc-o1] r[impl heap.free-cache]
    #[inline]
    /// # Safety
    /// - `ptr` must have been returned by `alloc` on some `Heap` sharing the
    ///   same `PagePool`.
    /// - `layout` must match the layout passed to the original `alloc` call.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        #[cfg(feature = "pprof")]
        if crate::pprof::should_sample() {
            crate::pprof::maybe_remove_sample(ptr.as_ptr());
        }

        let Some(idx) = size_class::class_index(layout) else {
            #[cfg(feature = "metrics")]
            self.metrics().on_large_dealloc(layout.size());
            // SAFETY: ptr was allocated via alloc_large with the same layout.
            return unsafe { self.pool.dealloc_large(ptr, layout) };
        };

        #[cfg(feature = "metrics")]
        self.metrics().on_dealloc(idx, size_class::class_size(idx));

        if let Some(active) = &mut self.bins[idx] {
            // SAFETY: ptr was allocated from a slab of this class.
            let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
            if active.as_ref().header_eq(slab_ref) {
                active.dealloc_local(ptr);
                return;
            }
        }

        if self.caches[idx].is_full() {
            self.flush_cache(idx);
        }
        self.caches[idx].push(ptr);
    }

    #[cfg(test)]
    fn drain_remote_all(&mut self) {
        for idx in 0..NUM_CLASSES {
            self.flush_cache(idx);
        }
        for slab in self.bins.iter_mut().flatten() {
            slab.drain_remote();
        }
        for idx in 0..NUM_CLASSES {
            for heads in [&self.full_heads, &self.partial_heads] {
                let mut cursor = heads[idx];
                while let Some(base) = cursor {
                    // SAFETY: base came from our full/partial list; slab is valid.
                    let mut slab = unsafe { Slab::from_raw(base) };
                    slab.drain_remote();
                    cursor = slab.next_link();
                }
            }
        }
    }

    /// Pop a slab from the partial list and install it as the active slab.
    /// Returns true if a partial slab was available.
    #[cold]
    #[inline(never)]
    fn try_partial(&mut self, class_idx: usize) -> bool {
        if let Some(slab) = slab_list_pop(&mut self.partial_heads[class_idx]) {
            self.bins[class_idx] = Some(slab);
            true
        } else {
            false
        }
    }

    /// Walk the full list for `class_idx`, draining remote frees. The first
    /// partial slab found is promoted directly to the active bin (fast path).
    /// Additional partial slabs are collected onto the partial list so
    /// subsequent allocs can pop them in O(1). Fully-free slabs are returned
    /// to the pool.
    #[cold]
    #[inline(never)]
    fn scan_full_list(&mut self, class_idx: usize) {
        // SAFETY: pointer to full_heads element; no aliasing access during iteration.
        let mut cursor = unsafe { SlabListCursor::new(&raw mut self.full_heads[class_idx]) };
        while let Some(base) = cursor.current() {
            // SAFETY: base came from our full list; slab is valid.
            let mut slab = unsafe { Slab::from_raw(base) };
            if !slab.has_pending_remote() {
                cursor.advance(&mut slab);
                continue;
            }
            slab.drain_remote();
            if slab.is_fully_free() {
                cursor.remove_current(&slab);
                self.pool.dealloc_slab(slab.into_raw());
                continue;
            }
            if slab.free_count() > 0 {
                cursor.remove_current(&slab);
                slab.set_next_link(None);
                if self.bins[class_idx].is_none() {
                    self.bins[class_idx] = Some(slab);
                } else {
                    slab_list_push(&mut self.partial_heads[class_idx], slab);
                }
                continue;
            }
            cursor.advance(&mut slab);
        }
    }

    /// Move the exhausted active slab to the full list for its class.
    #[cold]
    #[inline(never)]
    fn retire_active(&mut self, class_idx: usize) {
        if let Some(slab) = self.bins[class_idx].take() {
            slab_list_push(&mut self.full_heads[class_idx], slab);
        }
    }

    /// Flush the free cache for `class_idx`, returning cached pointers to
    /// their owning slabs. Own-slab entries go directly to the local free
    /// list (no atomics); remote entries are chained per-slab and pushed
    /// with a single CAS each.
    #[cold]
    #[inline(never)]
    fn flush_cache(&mut self, class_idx: usize) {
        let n = self.caches[class_idx].count as usize;
        if n == 0 {
            return;
        }

        #[cfg(feature = "metrics")]
        // Field access (not method call) so the borrow checker sees
        // self.metrics and self.caches as disjoint borrows.
        // SAFETY: UnsafeCell allows shared access; we hold no &mut Heap.
        unsafe { &*self.metrics.get() }.on_cache_flush(class_idx);

        // Sort entries by slab base so consecutive entries for the same slab
        // are adjacent. Insertion sort is fine for <=64 elements.
        let entries = &mut self.caches[class_idx].entries[..n];
        for i in 1..n {
            let key = entries[i];
            let mut j = i;
            while j > 0
                && (entries[j - 1].as_ptr() as usize & SLAB_MASK)
                    > (key.as_ptr() as usize & SLAB_MASK)
            {
                entries[j] = entries[j - 1];
                j -= 1;
            }
            entries[j] = key;
        }

        let mut i = 0;
        while i < n {
            let slab_base_addr = entries[i].as_ptr() as usize & SLAB_MASK;
            // SAFETY: slab_base_addr is slab-aligned; entries came from our allocator.
            let slab_base = unsafe { NonNull::new_unchecked(slab_base_addr as *mut SlabBase) };

            // Collect the run of entries for this slab.
            let run_start = i;
            i += 1;
            while i < n && (entries[i].as_ptr() as usize & SLAB_MASK) == slab_base_addr {
                i += 1;
            }

            // Check ownership via heap_id in the slab header.
            // SAFETY: entries came from our allocator; ptr is interior to a slab.
            let slab_ref = unsafe { SlabRef::from_interior_ptr(entries[run_start].as_ptr()) };
            if slab_ref.heap_id() == self.id {
                // Own slab: push each entry directly to the local free list.
                // SAFETY: slab_base derived from entries; slab is valid.
                let mut slab = unsafe { Slab::from_raw(slab_base) };
                for e in &entries[run_start..i] {
                    slab.dealloc_local(*e);
                }
            } else {
                // Remote slab: chain entries and push the chain in one CAS.
                #[cfg(feature = "metrics")]
                // SAFETY: UnsafeCell allows shared access; we hold no &mut Heap.
                unsafe { &*self.metrics.get() }.on_remote_free((i - run_start) as u64);
                for j in run_start..i - 1 {
                    // SAFETY: entries are valid alloc pointers; we write the free-list link.
                    unsafe { slab::write_next(entries[j], entries[j + 1].as_ptr()) };
                }
                slab_ref.push_chain_remote(entries[run_start], entries[i - 1]);
            }
        }

        self.caches[class_idx].count = 0;

        // Speculatively drain remote frees on the active slab. If the
        // caller is about to exhaust the slab and enter promote_retired,
        // this can recover slots and avoid the retired list walk entirely.
        if let Some(slab) = &mut self.bins[class_idx]
            && slab.has_pending_remote()
        {
            slab.drain_remote();
        }
    }

    /// Retire the active slab during thread exit. Fully-free slabs go back
    /// to the pool; partially-used slabs are abandoned for adoption.
    #[cold]
    #[inline(never)]
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
            self.flush_cache(idx);
            self.retire_slab(idx);
            // Drain both full and partial lists.
            for heads in [&mut self.full_heads, &mut self.partial_heads] {
                while let Some(mut slab) = slab_list_pop(&mut heads[idx]) {
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
        assert!(
            !first_slab_ref.header_eq(next_slab_ref),
            "should be a different slab"
        );

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

    // r[verify heap.page-queue]
    #[test]
    fn full_queue_scan_promotes_partial() {
        let pool = Arc::new(pool());
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();

        // Fill slab 1 completely → moves to full queue.
        let first_ptr = heap.alloc(layout).unwrap();
        let first_slab = unsafe { SlabRef::from_interior_ptr(first_ptr.as_ptr()) };
        let slot_count = first_slab.slot_count();
        let mut slab1_ptrs = vec![first_ptr];
        for _ in 1..slot_count {
            slab1_ptrs.push(heap.alloc(layout).unwrap());
        }

        // Slab 2 becomes active.
        let slab2_ptr = heap.alloc(layout).unwrap();
        let slab2 = unsafe { SlabRef::from_interior_ptr(slab2_ptr.as_ptr()) };
        assert!(!first_slab.header_eq(slab2));

        // Remote-free one slot from slab 1 (via another thread).
        let remote_raw = slab1_ptrs.pop().unwrap().as_ptr() as usize;
        std::thread::spawn(move || {
            let ptr = NonNull::new(remote_raw as *mut u8).unwrap();
            let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
            slab_ref.dealloc_remote(ptr);
        })
        .join()
        .unwrap();

        // Fill slab 2 completely → moves to full queue.
        let mut slab2_ptrs = vec![slab2_ptr];
        for _ in 1..slot_count {
            slab2_ptrs.push(heap.alloc(layout).unwrap());
        }

        // Next alloc: no active slab, cache empty, partial queue empty.
        // scan_full_list drains slab 1's remote free → slab 1 becomes partial
        // → promoted to active → alloc succeeds from slab 1.
        let recovered = heap.alloc(layout).unwrap();
        let recovered_slab = unsafe { SlabRef::from_interior_ptr(recovered.as_ptr()) };
        assert!(
            first_slab.header_eq(recovered_slab),
            "should allocate from promoted slab 1, not a new slab"
        );

        unsafe { heap.dealloc(recovered, layout) };
        for ptr in slab1_ptrs {
            unsafe { heap.dealloc(ptr, layout) };
        }
        for ptr in slab2_ptrs {
            unsafe { heap.dealloc(ptr, layout) };
        }
    }

    #[test]
    fn large_allocation_passthrough() {
        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(1 << 20, 4096).unwrap();
        let ptr = heap.alloc(layout).unwrap();
        assert_eq!(ptr.as_ptr() as usize % 4096, 0);
        unsafe { heap.dealloc(ptr, layout) };
    }

    // r[verify heap.thread-exit] r[verify slab.return-to-pool]
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

    // r[verify heap.identity] r[verify heap.dealloc-o1]
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

    // r[verify heap.free-cache]
    #[test]
    fn free_cache_absorbs_non_active_slab_frees() {
        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();

        // Fill the first slab to force a second one.
        let first_ptr = heap.alloc(layout).unwrap();
        let first_slab = unsafe { SlabRef::from_interior_ptr(first_ptr.as_ptr()) };
        let slot_count = first_slab.slot_count();

        let mut first_ptrs = vec![first_ptr];
        for _ in 1..slot_count {
            first_ptrs.push(heap.alloc(layout).unwrap());
        }

        // Now a second slab becomes active.
        let second_ptr = heap.alloc(layout).unwrap();
        let second_slab = unsafe { SlabRef::from_interior_ptr(second_ptr.as_ptr()) };
        assert!(!first_slab.header_eq(second_slab));

        // Free a pointer from the first (now retired) slab — goes to the cache.
        let cached = first_ptrs.pop().unwrap();
        let cached_addr = cached.as_ptr() as usize;
        unsafe { heap.dealloc(cached, layout) };

        // Alloc should return the cached pointer immediately.
        let recovered = heap.alloc(layout).unwrap();
        assert_eq!(recovered.as_ptr() as usize, cached_addr);

        // Clean up.
        unsafe { heap.dealloc(recovered, layout) };
        unsafe { heap.dealloc(second_ptr, layout) };
        for ptr in first_ptrs {
            unsafe { heap.dealloc(ptr, layout) };
        }
    }

    // r[verify heap.free-cache]
    #[test]
    fn free_cache_flush_returns_to_slabs() {
        let pool = Arc::new(pool());
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();
        let pool2 = Arc::clone(&pool);

        // Allocate from one slab, then free from another thread so the
        // pointers land in heap2's cache as remote entries.
        let ptrs: Vec<_> = (0..CACHE_CAP)
            .map(|_| heap.alloc(layout).unwrap())
            .collect();
        let raws: Vec<usize> = ptrs.iter().map(|p| p.as_ptr() as usize).collect();

        std::thread::spawn(move || {
            let mut heap2 = Heap::new(&pool2);
            for raw in &raws {
                let ptr = NonNull::new(*raw as *mut u8).unwrap();
                unsafe { heap2.dealloc(ptr, layout) };
            }
            // heap2 drops — cache flushed, entries pushed via remote CAS.
        })
        .join()
        .unwrap();

        // Original heap should recover all slots after draining remote.
        heap.drain_remote_all();
        for _ in 0..CACHE_CAP {
            let _p = heap.alloc(layout).unwrap();
        }
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
        assert!(
            slab_ref1.header_eq(slab_ref2),
            "heap2 should adopt heap1's slab"
        );

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
        assert!(
            old_slab_ref.header_eq(new_slab_ref),
            "should adopt the abandoned slab"
        );

        // Free everything
        for raw in &raw_ptrs {
            unsafe { heap2.dealloc(NonNull::new(*raw as *mut u8).unwrap(), layout) };
        }
        unsafe { heap2.dealloc(new_ptr, layout) };
    }

    // r[verify metrics.class-alloc-count] r[verify metrics.class-dealloc-count]
    // r[verify metrics.class-alloc-bytes] r[verify metrics.class-free-bytes]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_counter_accuracy() {
        use core::sync::atomic::Ordering::Relaxed;
        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();
        let idx = size_class::class_index(layout).unwrap();
        let cs = size_class::class_size(idx) as u64;

        let p1 = heap.alloc(layout).unwrap();
        let p2 = heap.alloc(layout).unwrap();
        assert_eq!(heap.metrics().class_alloc_count[idx].load(Relaxed), 2);
        assert_eq!(heap.metrics().class_alloc_bytes[idx].load(Relaxed), 2 * cs);
        assert_eq!(heap.metrics().alloc_bytes.load(Relaxed), 2 * cs);

        unsafe { heap.dealloc(p1, layout) };
        assert_eq!(heap.metrics().class_dealloc_count[idx].load(Relaxed), 1);
        assert_eq!(heap.metrics().free_bytes.load(Relaxed), cs);

        unsafe { heap.dealloc(p2, layout) };
        assert_eq!(heap.metrics().class_dealloc_count[idx].load(Relaxed), 2);
    }

    // r[verify metrics.cache-hit-count]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_cache_hit_counted() {
        use core::sync::atomic::Ordering::Relaxed;

        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();
        let idx = size_class::class_index(layout).unwrap();

        // Fill the first slab to force a second.
        let first_ptr = heap.alloc(layout).unwrap();
        let first_slab = unsafe { SlabRef::from_interior_ptr(first_ptr.as_ptr()) };
        let slot_count = first_slab.slot_count();
        let mut first_ptrs = vec![first_ptr];
        for _ in 1..slot_count {
            first_ptrs.push(heap.alloc(layout).unwrap());
        }

        // Second slab becomes active.
        let _second_ptr = heap.alloc(layout).unwrap();

        // Free from first slab → goes to cache.
        let cached = first_ptrs.pop().unwrap();
        unsafe { heap.dealloc(cached, layout) };

        let before = heap.metrics().cache_hit_count[idx].load(Relaxed);
        // Alloc should hit the cache.
        let _recovered = heap.alloc(layout).unwrap();
        assert_eq!(
            heap.metrics().cache_hit_count[idx].load(Relaxed),
            before + 1
        );
    }

    // r[verify metrics.large-alloc-count] r[verify metrics.large-alloc-bytes]
    // r[verify metrics.large-dealloc-bytes]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_large_alloc_tracked() {
        use core::sync::atomic::Ordering::Relaxed;

        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(1 << 20, 4096).unwrap();
        let ptr = heap.alloc(layout).unwrap();
        assert_eq!(heap.metrics().large_alloc_count.load(Relaxed), 1);
        assert_eq!(
            heap.metrics().large_alloc_bytes.load(Relaxed),
            (1 << 20) as u64
        );
        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(
            heap.metrics().large_dealloc_bytes.load(Relaxed),
            (1 << 20) as u64
        );
    }

    // r[verify metrics.global-snapshot] r[verify metrics.global-allocated]
    // r[verify metrics.global-active] r[verify metrics.global-mapped]
    #[cfg(feature = "metrics")]
    #[test]
    fn snapshot_aggregates_registered_heaps() {
        use crate::slab::SLAB_SIZE;

        let pool = pool();
        let mut h1 = Heap::new(&pool);
        let mut h2 = Heap::new(&pool);

        pool.register_heap(h1.metrics.get().cast_const());
        pool.register_heap(h2.metrics.get().cast_const());

        let layout = Layout::from_size_align(64, 1).unwrap();
        let idx = size_class::class_index(layout).unwrap();
        let cs = size_class::class_size(idx) as u64;

        let p1 = h1.alloc(layout).unwrap();
        let p2 = h2.alloc(layout).unwrap();

        let snap = pool.snapshot();
        assert_eq!(snap.class_alloc_count[idx], 2);
        assert_eq!(snap.alloc_bytes, 2 * cs);
        assert_eq!(snap.allocated, 2 * cs);
        assert_eq!(snap.class_live_count[idx], 2);
        // Each heap has 1 outstanding slab, so active >= 2 * SLAB_SIZE.
        assert!(snap.active >= 2 * SLAB_SIZE as u64);
        // Mapped must include at least one segment.
        assert!(snap.mapped >= crate::pool::SEGMENT_SIZE as u64);

        unsafe { h1.dealloc(p1, layout) };
        unsafe { h2.dealloc(p2, layout) };

        pool.deregister_heap(h1.metrics.get().cast_const());
        pool.deregister_heap(h2.metrics.get().cast_const());
    }

    // r[verify metrics.cache-flush-count]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_cache_flush_counted() {
        use core::sync::atomic::Ordering::Relaxed;

        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();
        let idx = size_class::class_index(layout).unwrap();

        let first_ptr = heap.alloc(layout).unwrap();
        let first_slab = unsafe { SlabRef::from_interior_ptr(first_ptr.as_ptr()) };
        let slot_count = first_slab.slot_count();
        let mut first_ptrs = vec![first_ptr];
        for _ in 1..slot_count {
            first_ptrs.push(heap.alloc(layout).unwrap());
        }
        let _second_ptr = heap.alloc(layout).unwrap();

        // Fill the cache (CACHE_CAP entries), then one more to trigger flush.
        for _ in 0..=CACHE_CAP {
            let cached = first_ptrs.pop().unwrap();
            unsafe { heap.dealloc(cached, layout) };
        }

        assert!(heap.metrics().cache_flush_count[idx].load(Relaxed) > 0);
    }

    // r[verify metrics.segment-munmap-count]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_segment_munmap_counted() {
        use crate::slab::SLAB_SIZE;
        let pool = pool();

        let slabs_per_seg = crate::pool::SEGMENT_SIZE / SLAB_SIZE;
        let mut slabs = Vec::new();
        for _ in 0..slabs_per_seg {
            slabs.push(pool.alloc_slab().unwrap());
        }
        for slab in slabs {
            pool.dealloc_slab(slab);
        }

        let snap = pool.snapshot();
        assert!(snap.segment_munmap_count > 0);
    }

    // r[verify metrics.abandon-count] r[verify metrics.adopt-count]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_abandon_adopt_counted() {
        let pool = pool();
        let layout = Layout::from_size_align(64, 1).unwrap();
        let idx = size_class::class_index(layout).unwrap();

        let outstanding;
        {
            let mut heap1 = Heap::new(&pool);
            outstanding = heap1.alloc(layout).unwrap();
        }
        {
            let snap = pool.snapshot();
            assert!(snap.abandon_count[idx] > 0);
        }

        let mut heap2 = Heap::new(&pool);
        let _ptr = heap2.alloc(layout).unwrap();
        {
            let snap = pool.snapshot();
            assert!(snap.adopt_count[idx] > 0);
        }

        unsafe { heap2.dealloc(outstanding, layout) };
    }

    // r[verify metrics.thread-active-bytes]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_active_bytes_tracked() {
        use core::sync::atomic::Ordering::Relaxed;

        let pool = pool();
        let mut heap = Heap::new(&pool);
        let layout = Layout::from_size_align(64, 1).unwrap();
        let cs = size_class::class_size(size_class::class_index(layout).unwrap()) as u64;

        let ptr = heap.alloc(layout).unwrap();
        assert_eq!(heap.metrics().alloc_bytes.load(Relaxed), cs);
        assert_eq!(heap.metrics().free_bytes.load(Relaxed), 0);

        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(heap.metrics().free_bytes.load(Relaxed), cs);
    }

    // r[verify metrics.pool-lock-count] r[verify metrics.segment-mmap-count]
    // r[verify metrics.slab-purge-count]
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_pool_counters() {
        let pool = pool();

        // Alloc and free a slab directly to exercise pool-level metrics.
        let slab = pool.alloc_slab().unwrap();
        pool.dealloc_slab(slab);

        let snap = pool.snapshot();
        assert!(snap.pool_lock_count > 0);
        assert!(snap.segment_mmap_count > 0);
        assert!(snap.slab_purge_count > 0);
    }
}
