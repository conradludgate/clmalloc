//! Allocation metrics for observability and production debugging.
//!
//! Counter fields use `AtomicU64` with `Relaxed` ordering. Each `HeapMetrics`
//! has a single writer (the owning thread), so the load-add-store pattern is
//! safe without `fetch_add`. Snapshot readers on other threads see
//! slightly-stale-but-valid values via `Relaxed` loads.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::size_class::NUM_CLASSES;

pub(crate) const MAX_HEAPS: usize = 1024;

const R: Ordering = Ordering::Relaxed;

/// Single-writer atomic increment. Safe when exactly one thread writes
/// to this counter (load + add + store is not a race because there is
/// no concurrent writer).
#[inline(always)]
fn inc(a: &AtomicU64, v: u64) {
    a.store(a.load(R) + v, R);
}

// -- Per-heap counters -------------------------------------------------------

// r[impl metrics.thread-alloc-bytes] r[impl metrics.thread-free-bytes]
// r[impl metrics.thread-active-bytes]
// r[impl metrics.class-alloc-count] r[impl metrics.class-dealloc-count]
// r[impl metrics.class-alloc-bytes] r[impl metrics.class-free-bytes]
// r[impl metrics.large-alloc-count] r[impl metrics.large-alloc-bytes]
// r[impl metrics.large-dealloc-bytes]
// r[impl metrics.remote-free-count]
// r[impl metrics.histogram-storage]
pub(crate) struct HeapMetrics {
    /// Slot index in the pool's heap registry. Written under the pool lock
    /// during register/deregister (swap-remove updates the moved entry).
    pub registry_idx: AtomicU32,
    pub alloc_bytes: AtomicU64,
    pub free_bytes: AtomicU64,
    pub class_alloc_count: [AtomicU64; NUM_CLASSES],
    pub class_dealloc_count: [AtomicU64; NUM_CLASSES],
    pub class_alloc_bytes: [AtomicU64; NUM_CLASSES],
    pub class_free_bytes: [AtomicU64; NUM_CLASSES],
    pub remote_free_count: AtomicU64,
    pub large_alloc_count: AtomicU64,
    pub large_alloc_bytes: AtomicU64,
    pub large_dealloc_bytes: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_ARRAY: [AtomicU64; NUM_CLASSES] = [const { AtomicU64::new(0) }; NUM_CLASSES];

impl HeapMetrics {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const ZERO: Self = Self {
        registry_idx: AtomicU32::new(u32::MAX),
        alloc_bytes: AtomicU64::new(0),
        free_bytes: AtomicU64::new(0),
        class_alloc_count: ZERO_ARRAY,
        class_dealloc_count: ZERO_ARRAY,
        class_alloc_bytes: ZERO_ARRAY,
        class_free_bytes: ZERO_ARRAY,
        remote_free_count: AtomicU64::new(0),
        large_alloc_count: AtomicU64::new(0),
        large_alloc_bytes: AtomicU64::new(0),
        large_dealloc_bytes: AtomicU64::new(0),
    };

    #[inline(always)]
    pub fn on_alloc(&self, idx: usize, class_size: usize) {
        let size = class_size as u64;
        inc(&self.alloc_bytes, size);
        inc(&self.class_alloc_count[idx], 1);
        inc(&self.class_alloc_bytes[idx], size);
    }

    #[inline(always)]
    pub fn on_dealloc(&self, idx: usize, class_size: usize) {
        let size = class_size as u64;
        inc(&self.free_bytes, size);
        inc(&self.class_dealloc_count[idx], 1);
        inc(&self.class_free_bytes[idx], size);
    }

    #[inline(always)]
    pub fn on_large_alloc(&self, size: usize) {
        inc(&self.alloc_bytes, size as u64);
        inc(&self.large_alloc_count, 1);
        inc(&self.large_alloc_bytes, size as u64);
    }

    #[inline(always)]
    pub fn on_large_dealloc(&self, size: usize) {
        inc(&self.free_bytes, size as u64);
        inc(&self.large_dealloc_bytes, size as u64);
    }

    #[inline(always)]
    pub fn on_remote_free(&self, count: u64) {
        inc(&self.remote_free_count, count);
    }

    /// Fold another `HeapMetrics` into this one (used for dead-heap accumulation
    /// under the pool lock — no concurrent writer on either side).
    pub fn accumulate(&mut self, other: &HeapMetrics) {
        *self.alloc_bytes.get_mut() += other.alloc_bytes.load(R);
        *self.free_bytes.get_mut() += other.free_bytes.load(R);
        *self.remote_free_count.get_mut() += other.remote_free_count.load(R);
        *self.large_alloc_count.get_mut() += other.large_alloc_count.load(R);
        *self.large_alloc_bytes.get_mut() += other.large_alloc_bytes.load(R);
        *self.large_dealloc_bytes.get_mut() += other.large_dealloc_bytes.load(R);
        for i in 0..NUM_CLASSES {
            *self.class_alloc_count[i].get_mut() += other.class_alloc_count[i].load(R);
            *self.class_dealloc_count[i].get_mut() += other.class_dealloc_count[i].load(R);
            *self.class_alloc_bytes[i].get_mut() += other.class_alloc_bytes[i].load(R);
            *self.class_free_bytes[i].get_mut() += other.class_free_bytes[i].load(R);
        }
    }
}

// -- Pool-level metrics + heap registry --------------------------------------

// r[impl metrics.abandon-count] r[impl metrics.adopt-count]
// r[impl metrics.segment-mmap-count] r[impl metrics.segment-munmap-count]
// r[impl metrics.slab-purge-count]
// r[impl metrics.pool-lock-count]
pub(crate) struct PoolMetrics {
    pub abandon_count: [u64; NUM_CLASSES],
    pub adopt_count: [u64; NUM_CLASSES],
    pub segment_mmap_count: u64,
    pub segment_munmap_count: u64,
    pub slab_purge_count: u64,
    pub pool_lock_count: u64,
    heap_ptrs: [*const HeapMetrics; MAX_HEAPS],
    heap_count: usize,
    dead: HeapMetrics,
}

// SAFETY: Only accessed under the pool's spin lock. Contains raw pointers
// (*const HeapMetrics) which are !Send, but the lock ensures exclusive access.
unsafe impl Send for PoolMetrics {}

impl PoolMetrics {
    pub const fn new() -> Self {
        Self {
            abandon_count: [0; NUM_CLASSES],
            adopt_count: [0; NUM_CLASSES],
            segment_mmap_count: 0,
            segment_munmap_count: 0,
            slab_purge_count: 0,
            pool_lock_count: 0,
            heap_ptrs: [core::ptr::null(); MAX_HEAPS],
            heap_count: 0,
            dead: HeapMetrics::ZERO,
        }
    }

    pub fn register_heap(&mut self, ptr: *const HeapMetrics) {
        assert!(self.heap_count < MAX_HEAPS, "heap registry full");
        let idx = self.heap_count;
        self.heap_ptrs[idx] = ptr;
        self.heap_count += 1;
        // SAFETY: ptr is valid — the heap was just constructed and placed
        // at its final address. registry_idx is AtomicU32 so this write
        // through a shared reference is sound.
        #[allow(clippy::cast_possible_truncation)]
        unsafe { &*ptr }.registry_idx.store(idx as u32, R);
    }

    pub fn deregister_heap(&mut self, ptr: *const HeapMetrics) {
        // SAFETY: ptr is valid — the heap is alive and being dropped.
        // The owning thread is in its destructor, so no concurrent writes
        // to the counter fields.
        let metrics = unsafe { &*ptr };
        let idx = metrics.registry_idx.load(R) as usize;
        debug_assert_eq!(self.heap_ptrs[idx], ptr);
        self.dead.accumulate(metrics);
        self.heap_count -= 1;
        if idx != self.heap_count {
            self.heap_ptrs[idx] = self.heap_ptrs[self.heap_count];
            // SAFETY: the moved entry's heap is alive and registered.
            // registry_idx is AtomicU32 so this write is sound.
            #[allow(clippy::cast_possible_truncation)]
            unsafe { &*self.heap_ptrs[idx] }
                .registry_idx
                .store(idx as u32, R);
        }
        self.heap_ptrs[self.heap_count] = core::ptr::null();
    }

    /// Sum dead + live heap metrics into `snapshot`.
    pub fn aggregate_heap_metrics(&self, snapshot: &mut MetricsSnapshot) {
        snapshot.accumulate_heap(&self.dead);
        for i in 0..self.heap_count {
            // SAFETY: Registered heaps are alive. Counter reads use
            // AtomicU64::load(Relaxed), which is sound even when the
            // owning thread is concurrently writing via store(Relaxed).
            let metrics = unsafe { &*self.heap_ptrs[i] };
            snapshot.accumulate_heap(metrics);
        }

        snapshot.abandon_count = self.abandon_count;
        snapshot.adopt_count = self.adopt_count;
        snapshot.segment_mmap_count = self.segment_mmap_count;
        snapshot.segment_munmap_count = self.segment_munmap_count;
        snapshot.slab_purge_count = self.slab_purge_count;
        snapshot.pool_lock_count = self.pool_lock_count;
    }
}

// -- Snapshot -----------------------------------------------------------------

// r[impl metrics.global-snapshot] r[impl metrics.global-allocated]
// r[impl metrics.global-active] r[impl metrics.global-mapped]
// r[impl metrics.class-live] r[impl metrics.class-slab-count]
pub struct MetricsSnapshot {
    pub allocated: u64,
    pub active: u64,
    pub mapped: u64,

    pub alloc_bytes: u64,
    pub free_bytes: u64,

    pub class_alloc_count: [u64; NUM_CLASSES],
    pub class_dealloc_count: [u64; NUM_CLASSES],
    pub class_alloc_bytes: [u64; NUM_CLASSES],
    pub class_free_bytes: [u64; NUM_CLASSES],
    pub class_live_count: [i64; NUM_CLASSES],
    pub class_live_bytes: [i64; NUM_CLASSES],

    pub abandon_count: [u64; NUM_CLASSES],
    pub adopt_count: [u64; NUM_CLASSES],

    pub segment_mmap_count: u64,
    pub segment_munmap_count: u64,
    pub slab_purge_count: u64,

    pub large_alloc_count: u64,
    pub large_alloc_bytes: u64,
    pub large_dealloc_bytes: u64,

    pub remote_free_count: u64,

    pub pool_lock_count: u64,
}

impl MetricsSnapshot {
    pub(crate) fn new() -> Self {
        Self {
            allocated: 0,
            active: 0,
            mapped: 0,
            alloc_bytes: 0,
            free_bytes: 0,
            class_alloc_count: [0; NUM_CLASSES],
            class_dealloc_count: [0; NUM_CLASSES],
            class_alloc_bytes: [0; NUM_CLASSES],
            class_free_bytes: [0; NUM_CLASSES],
            class_live_count: [0; NUM_CLASSES],
            class_live_bytes: [0; NUM_CLASSES],
            abandon_count: [0; NUM_CLASSES],
            adopt_count: [0; NUM_CLASSES],
            segment_mmap_count: 0,
            segment_munmap_count: 0,
            slab_purge_count: 0,
            large_alloc_count: 0,
            large_alloc_bytes: 0,
            large_dealloc_bytes: 0,
            remote_free_count: 0,
            pool_lock_count: 0,
        }
    }

    fn accumulate_heap(&mut self, h: &HeapMetrics) {
        self.alloc_bytes += h.alloc_bytes.load(R);
        self.free_bytes += h.free_bytes.load(R);
        self.remote_free_count += h.remote_free_count.load(R);
        self.large_alloc_count += h.large_alloc_count.load(R);
        self.large_alloc_bytes += h.large_alloc_bytes.load(R);
        self.large_dealloc_bytes += h.large_dealloc_bytes.load(R);
        for i in 0..NUM_CLASSES {
            self.class_alloc_count[i] += h.class_alloc_count[i].load(R);
            self.class_dealloc_count[i] += h.class_dealloc_count[i].load(R);
            self.class_alloc_bytes[i] += h.class_alloc_bytes[i].load(R);
            self.class_free_bytes[i] += h.class_free_bytes[i].load(R);
        }
    }

    pub(crate) fn finalize(&mut self) {
        for i in 0..NUM_CLASSES {
            self.class_live_count[i] =
                self.class_alloc_count[i].cast_signed() - self.class_dealloc_count[i].cast_signed();
            self.class_live_bytes[i] =
                self.class_alloc_bytes[i].cast_signed() - self.class_free_bytes[i].cast_signed();
        }
        self.allocated = self.alloc_bytes.saturating_sub(self.free_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::size_class;

    // r[verify metrics.thread-alloc-bytes] r[verify metrics.thread-free-bytes]
    #[test]
    fn heap_metrics_on_alloc_dealloc() {
        let m = HeapMetrics::ZERO;
        let idx = 5;
        let cs = 128;
        m.on_alloc(idx, cs);
        assert_eq!(m.alloc_bytes.load(R), 128);
        assert_eq!(m.class_alloc_count[idx].load(R), 1);
        assert_eq!(m.class_alloc_bytes[idx].load(R), 128);

        m.on_dealloc(idx, cs);
        assert_eq!(m.free_bytes.load(R), 128);
        assert_eq!(m.class_dealloc_count[idx].load(R), 1);
        assert_eq!(m.class_free_bytes[idx].load(R), 128);
    }

    #[test]
    fn accumulate_folds_counters() {
        let mut a = HeapMetrics::ZERO;
        let b = HeapMetrics::ZERO;
        a.on_alloc(0, 8);
        a.on_alloc(0, 8);
        b.on_alloc(0, 8);
        b.on_dealloc(0, 8);

        a.accumulate(&b);
        assert_eq!(a.alloc_bytes.load(R), 24);
        assert_eq!(a.free_bytes.load(R), 8);
        assert_eq!(a.class_alloc_count[0].load(R), 3);
    }

    // r[verify metrics.class-live]
    #[test]
    fn finalize_computes_live_counts() {
        let mut snap = MetricsSnapshot::new();
        snap.class_alloc_count[3] = 10;
        snap.class_dealloc_count[3] = 4;
        snap.class_alloc_bytes[3] = 640;
        snap.class_free_bytes[3] = 256;
        snap.alloc_bytes = 640;
        snap.free_bytes = 256;
        snap.finalize();
        assert_eq!(snap.class_live_count[3], 6);
        assert_eq!(snap.class_live_bytes[3], 384);
        assert_eq!(snap.allocated, 384);
    }

    // r[verify metrics.histogram-storage]
    #[test]
    fn per_class_counters_are_independent() {
        let mut snap = MetricsSnapshot::new();
        snap.class_alloc_count[0] = 5;
        let idx_64 =
            size_class::class_index(core::alloc::Layout::from_size_align(64, 1).unwrap()).unwrap();
        snap.class_alloc_count[idx_64] = 3;

        assert_eq!(snap.class_alloc_count[0], 5);
        assert_eq!(snap.class_alloc_count[idx_64], 3);
        for i in 0..NUM_CLASSES {
            if i != 0 && i != idx_64 {
                assert_eq!(snap.class_alloc_count[i], 0);
            }
        }
    }
}
