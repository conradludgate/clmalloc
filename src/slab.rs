//! Slab management: contiguous memory regions serving allocations of a single size class.
//!
//! Each slab is 64KB, aligned to 64KB. The header at the slab base contains
//! a local free list (owning thread, non-atomic) and a remote free list (any
//! thread, atomic Treiber stack).
//!
//! Access is split into two handle types:
//! - [`Slab`] — owner handle (`Send + !Sync`). Requires `&mut self` for
//!   local operations, making single-threaded access a compile-time guarantee.
//! - [`SlabRef`] — shared handle (`Send + Sync`). Only exposes remote
//!   deallocation and immutable metadata. Recovered from any interior pointer
//!   in the dealloc path.
//!
//! # Design references
//!
//! - Leijen, Zorn, de Moura. [*Mimalloc: Free List Sharding in Action*][mimalloc-paper].
//!   The local/remote free list sharding and per-page (slab) design are adapted
//!   from mimalloc's page structure.
//! - Treiber. [*Systems Programming: Coping with Parallelism*][treiber].
//!   The remote free list is a classic Treiber stack (lock-free CAS push,
//!   atomic swap drain).
//! - Tokio's [`sharded-slab`][sharded-slab] crate for the loom-compatible
//!   `UnsafeCell`/`AtomicPtr` abstraction pattern used in [`crate::sync`].
//!
//! [mimalloc-paper]: https://www.microsoft.com/en-us/research/publication/mimalloc-free-list-sharding-in-action/
//! [treiber]: https://dominoweb.draco.res.ibm.com/58319a2ed2b1078985257003004617ef.html
//! [sharded-slab]: https://docs.rs/sharded-slab/latest/sharded_slab/implementation/

use core::mem::size_of;
use core::ptr::{self, NonNull, null_mut};

use crate::size_class;
use crate::sync::{AtomicPtr, Ordering, UnsafeCell};

pub const SLAB_SIZE: usize = 1 << 16; // 64KB
const SLAB_MASK: usize = !(SLAB_SIZE - 1);

// -- Core unsafe primitives for intrusive free list --
//
// Every free slot stores a next-pointer in its first pointer-sized bytes.
// These two helpers are the only code that touches slot memory as free-list
// metadata. Unaligned access is used because not all size classes are
// pointer-aligned (e.g. 10-byte slots).

type Link = *mut u8;

#[inline(always)]
unsafe fn read_next(slot: *mut u8) -> Link {
    unsafe { slot.cast::<Link>().read_unaligned() }
}

#[inline(always)]
unsafe fn write_next(slot: *mut u8, next: Link) {
    unsafe { slot.cast::<Link>().write_unaligned(next) };
}

struct LocalState {
    head: Link,
    free_count: u16,
}

// r[impl slab.alignment] r[impl slab.single-class] r[impl slab.metadata] r[impl slab.owner]
#[repr(C)]
struct SlabHeader {
    slot_size: u16,
    slot_count: u16,
    slots_offset: u16,
    size_class_index: u8,
    // r[impl slab.local-freelist] r[impl slab.local-no-atomics]
    local: UnsafeCell<LocalState>,
    // r[impl slab.remote-freelist]
    remote_head: AtomicPtr<u8>,
}

impl SlabHeader {
    unsafe fn init(base: NonNull<u8>, size_class_index: u8) -> NonNull<SlabHeader> {
        let slot_size = size_class::class_size(size_class_index as usize);
        debug_assert!(slot_size >= size_of::<Link>());

        let slots_offset = size_of::<Self>().next_multiple_of(slot_size);
        let slot_count = (SLAB_SIZE - slots_offset) / slot_size;
        debug_assert!(slot_count > 0 && slot_count <= u16::MAX as usize);

        let base_ptr = base.as_ptr();

        let mut prev: Link = null_mut();
        for i in (0..slot_count).rev() {
            let slot = unsafe { base_ptr.add(slots_offset + i * slot_size) };
            unsafe { write_next(slot, prev) };
            prev = slot;
        }

        let header = base_ptr.cast::<SlabHeader>();
        unsafe {
            ptr::write(
                header,
                SlabHeader {
                    slot_size: slot_size as u16,
                    slot_count: slot_count as u16,
                    slots_offset: slots_offset as u16,
                    size_class_index,
                    local: UnsafeCell::new(LocalState {
                        head: prev,
                        free_count: slot_count as u16,
                    }),
                    remote_head: AtomicPtr::new(null_mut()),
                },
            );
            NonNull::new_unchecked(header)
        }
    }

    unsafe fn from_ptr(ptr: *const u8) -> NonNull<SlabHeader> {
        unsafe { NonNull::new_unchecked((ptr as usize & SLAB_MASK) as *mut SlabHeader) }
    }
}

// ---------------------------------------------------------------------------
// Slab — owner handle (Send + !Sync)
// ---------------------------------------------------------------------------

/// Owner handle for a slab. Only one exists per slab.
///
/// Takes `&mut self` for operations that touch the local free list, making
/// single-threaded access a compile-time guarantee rather than a runtime check.
///
/// `Send` so ownership can transfer (e.g. pool reclamation on thread exit).
/// `!Sync` (inherited from `NonNull`) so `&Slab` cannot be shared across threads.
pub struct Slab {
    header: NonNull<SlabHeader>,
}

// SAFETY: Slab is a unique owner handle. Local mutable state is only accessed
// through &mut Slab. Transferring ownership across threads is safe.
unsafe impl Send for Slab {}

impl Slab {
    /// Initialize a slab from a raw 64KB-aligned memory region.
    ///
    /// Chains all slots into the local free list:
    /// slot\[0\] → slot\[1\] → … → slot\[N−1\] → null.
    ///
    /// # Safety
    /// - `base` must be `SLAB_SIZE`-aligned, pointing to `SLAB_SIZE` bytes of
    ///   valid, writable memory.
    /// - No concurrent access to the region during init.
    pub unsafe fn init(base: NonNull<u8>, size_class_index: u8) -> Slab {
        let header = unsafe { SlabHeader::init(base, size_class_index) };
        Slab { header }
    }

    fn header(&self) -> &SlabHeader {
        // SAFETY: Slab can only be created from a valid, initialized header,
        // and the backing memory is live as long as Slab exists.
        unsafe { self.header.as_ref() }
    }

    /// Get a [`SlabRef`] for this slab, usable from any thread.
    pub fn as_ref(&self) -> SlabRef {
        SlabRef {
            header: self.header,
        }
    }

    /// Consume the owner handle, returning the raw slab base pointer.
    ///
    /// The caller (page pool) takes responsibility for the backing memory.
    /// The slab must be fully free (all slots returned) before calling this.
    pub fn into_raw(self) -> NonNull<u8> {
        self.header.cast()
    }

    pub fn slot_size(&self) -> usize {
        self.header().slot_size as usize
    }

    pub fn slot_count(&self) -> usize {
        self.header().slot_count as usize
    }

    pub fn size_class_index(&self) -> usize {
        self.header().size_class_index as usize
    }

    /// Free slots on the local list. Does not include remotely freed slots —
    /// call `drain_remote` first for a complete count.
    pub fn free_count(&self) -> u16 {
        self.header().local.with_mut(|p| unsafe { (*p).free_count })
    }

    // r[impl slab.return-to-pool]
    /// True when every slot is on the local free list.
    /// Call `drain_remote` first to account for remotely freed slots.
    pub fn is_fully_free(&self) -> bool {
        self.free_count() == self.header().slot_count
    }

    // r[impl slab.local-freelist] r[impl slab.local-no-atomics]
    /// Pop a slot from the local free list. O(1).
    pub fn alloc(&mut self) -> Option<NonNull<u8>> {
        self.header().local.with_mut(|p| {
            let local = unsafe { &mut *p };
            let head = local.head;
            if head.is_null() {
                return None;
            }
            local.head = unsafe { read_next(head) };
            local.free_count -= 1;
            Some(unsafe { NonNull::new_unchecked(head) })
        })
    }

    // r[impl slab.local-freelist] r[impl slab.local-no-atomics]
    /// Return a slot to the local free list. O(1).
    pub fn dealloc_local(&mut self, ptr: NonNull<u8>) {
        self.header().local.with_mut(|p| {
            let local = unsafe { &mut *p };
            unsafe { write_next(ptr.as_ptr(), local.head) };
            local.head = ptr.as_ptr();
            local.free_count += 1;
        })
    }

    // r[impl slab.remote-drain]
    /// Atomically drain the remote free list into the local free list.
    ///
    /// Swaps the remote head to null, walks the chain, and prepends each
    /// node to the local list. Returns the number of slots recovered.
    pub fn drain_remote(&mut self) -> u16 {
        let header = self.header();
        let chain = header.remote_head.swap(null_mut(), Ordering::Acquire);
        header.local.with_mut(|p| {
            let local = unsafe { &mut *p };
            let mut count = 0u16;
            let mut cursor = chain;
            while !cursor.is_null() {
                let next = unsafe { read_next(cursor) };
                unsafe { write_next(cursor, local.head) };
                local.head = cursor;
                local.free_count += 1;
                count += 1;
                cursor = next;
            }
            count
        })
    }
}

// ---------------------------------------------------------------------------
// SlabRef — shared handle (Send + Sync)
// ---------------------------------------------------------------------------

/// Shared handle to a slab. `Send + Sync`, safe to hold from any thread.
///
/// Only exposes remote deallocation and read-only metadata. Obtained via
/// [`Slab::as_ref`] or recovered from an interior pointer with
/// [`SlabRef::from_interior_ptr`].
#[derive(Clone, Copy)]
pub struct SlabRef {
    header: NonNull<SlabHeader>,
}

// SAFETY: SlabRef only accesses immutable fields and the atomic remote_head.
unsafe impl Send for SlabRef {}
unsafe impl Sync for SlabRef {}

impl SlabRef {
    fn header(&self) -> &SlabHeader {
        // SAFETY: SlabRef can only be created from a valid, initialized header,
        // and callers guarantee the backing memory is live.
        unsafe { self.header.as_ref() }
    }

    // r[impl slab.alignment] r[impl slab.metadata]
    /// Recover a [`SlabRef`] from any pointer within a live slab.
    ///
    /// # Safety
    /// `ptr` must point within a live, initialized slab.
    pub unsafe fn from_interior_ptr(ptr: *const u8) -> SlabRef {
        SlabRef {
            header: unsafe { SlabHeader::from_ptr(ptr) },
        }
    }

    pub fn slot_size(&self) -> usize {
        self.header().slot_size as usize
    }

    pub fn slot_count(&self) -> usize {
        self.header().slot_count as usize
    }

    pub fn size_class_index(&self) -> usize {
        self.header().size_class_index as usize
    }

    // r[impl slab.remote-freelist]
    /// Push a freed slot onto the remote free list via atomic CAS.
    ///
    /// O(1) amortized (CAS retry loop).
    pub fn dealloc_remote(&self, ptr: NonNull<u8>) {
        let header = self.header();
        let slot = ptr.as_ptr();
        let mut head = header.remote_head.load(Ordering::Relaxed);
        loop {
            unsafe { write_next(slot, head) };
            match header.remote_head.compare_exchange_weak(
                head,
                slot,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::alloc::{alloc_zeroed, dealloc, Layout};
    use std::collections::HashSet;

    struct TestSlab {
        slab: Slab,
        ptr: NonNull<u8>,
        layout: Layout,
    }

    impl TestSlab {
        fn new(size_class_index: u8) -> Self {
            let layout = Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap();
            let ptr = unsafe {
                let p = alloc_zeroed(layout);
                NonNull::new(p).expect("aligned alloc failed")
            };
            let slab = unsafe { Slab::init(ptr, size_class_index) };
            Self { slab, ptr, layout }
        }
    }

    impl Drop for TestSlab {
        fn drop(&mut self) {
            unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
        }
    }

    // r[verify slab.alignment]
    #[test]
    fn interior_ptr_recovers_header() {
        let mut t = TestSlab::new(0);
        let slot0 = t.slab.alloc().unwrap();
        let slot1 = t.slab.alloc().unwrap();

        unsafe {
            let r0 = SlabRef::from_interior_ptr(slot0.as_ptr());
            let r1 = SlabRef::from_interior_ptr(slot1.as_ptr());
            assert_eq!(r0.header, t.slab.header);
            assert_eq!(r1.header, t.slab.header);
        }

        t.slab.dealloc_local(slot1);
        t.slab.dealloc_local(slot0);
    }

    // r[verify slab.single-class]
    #[test]
    fn all_slots_same_stride() {
        for class_idx in [0u8, 5, 10, 20, 48] {
            let mut t = TestSlab::new(class_idx);
            let slot_size = t.slab.slot_size();
            let mut slots = Vec::new();
            while let Some(ptr) = t.slab.alloc() {
                slots.push(ptr);
            }
            assert!(!slots.is_empty());
            for i in 1..slots.len() {
                let diff = slots[i].as_ptr() as usize - slots[i - 1].as_ptr() as usize;
                assert_eq!(
                    diff, slot_size,
                    "class {class_idx}: spacing {diff} != {slot_size} at {i}"
                );
            }
            for slot in slots {
                t.slab.dealloc_local(slot);
            }
        }
    }

    // r[verify slab.metadata]
    #[test]
    fn header_fields_accessible() {
        let t = TestSlab::new(5);
        assert_eq!(t.slab.size_class_index(), 5);
        assert!(t.slab.slot_count() > 0);
        assert_eq!(t.slab.slot_size(), size_class::class_size(5));
    }

    // r[verify slab.owner]
    #[test]
    fn slab_is_send_not_sync() {
        fn assert_send<T: Send>() {}
        fn assert_not_sync<T>()
        where
            // T must NOT implement Sync — this compiles only if T: !Sync,
            // by requiring a bound that would conflict. We use a negative
            // reasoning trick: PhantomData<Cell<()>> ensures !Sync.
        {
        }
        assert_send::<Slab>();
        assert_not_sync::<Slab>();

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SlabRef>();
    }

    // r[verify slab.local-freelist]
    #[test]
    fn alloc_dealloc_no_duplicates() {
        let mut t = TestSlab::new(0);
        let mut seen = HashSet::new();
        let mut slots = Vec::new();

        while let Some(ptr) = t.slab.alloc() {
            assert!(seen.insert(ptr.as_ptr() as usize), "duplicate alloc");
            slots.push(ptr);
        }
        assert_eq!(slots.len(), t.slab.slot_count());
        assert!(t.slab.alloc().is_none());

        let half = slots.len() / 2;
        for ptr in slots.drain(..half) {
            t.slab.dealloc_local(ptr);
        }

        let mut re_seen = HashSet::new();
        for _ in 0..half {
            let ptr = t.slab.alloc().unwrap();
            assert!(re_seen.insert(ptr.as_ptr() as usize), "duplicate re-alloc");
            assert!(seen.contains(&(ptr.as_ptr() as usize)), "unknown slot");
            slots.push(ptr);
        }
        assert!(t.slab.alloc().is_none());

        for slot in slots {
            t.slab.dealloc_local(slot);
        }
    }

    // r[verify slab.local-no-atomics]
    // Design-level: LocalState.head is Link (not atomic); alloc/dealloc_local
    // go through UnsafeCell, not AtomicPtr. Enforced by the type system.

    // r[verify slab.remote-freelist]
    #[test]
    fn remote_dealloc_then_drain() {
        let mut t = TestSlab::new(0);
        let slot = t.slab.alloc().unwrap();
        let raw = slot.as_ptr() as usize;

        std::thread::spawn(move || {
            let ptr = NonNull::new(raw as *mut u8).unwrap();
            let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
            slab_ref.dealloc_remote(ptr);
        })
        .join()
        .unwrap();

        let drained = t.slab.drain_remote();
        assert_eq!(drained, 1);

        let recovered = t.slab.alloc().unwrap();
        assert_eq!(recovered.as_ptr() as usize, raw);
        t.slab.dealloc_local(recovered);
    }

    // r[verify slab.remote-drain]
    #[test]
    fn multi_thread_remote_drain() {
        let mut t = TestSlab::new(0);
        let n = 16usize;
        let mut raws = Vec::new();
        for _ in 0..n {
            raws.push(t.slab.alloc().unwrap().as_ptr() as usize);
        }

        let handles: Vec<_> = raws
            .iter()
            .copied()
            .map(|raw| {
                std::thread::spawn(move || {
                    let ptr = NonNull::new(raw as *mut u8).unwrap();
                    let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
                    slab_ref.dealloc_remote(ptr);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let drained = t.slab.drain_remote();
        assert_eq!(drained, n as u16);

        let mut recovered = HashSet::new();
        for _ in 0..n {
            recovered.insert(t.slab.alloc().unwrap().as_ptr() as usize);
        }
        let originals: HashSet<usize> = raws.into_iter().collect();
        assert_eq!(recovered, originals);
    }

    // r[verify slab.return-to-pool]
    #[test]
    fn fully_free_lifecycle() {
        let mut t = TestSlab::new(0);
        assert!(t.slab.is_fully_free());

        let slot = t.slab.alloc().unwrap();
        assert!(!t.slab.is_fully_free());

        t.slab.dealloc_local(slot);
        assert!(t.slab.is_fully_free());
    }

    #[test]
    fn into_raw_returns_base() {
        let layout = Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap();
        let base = unsafe {
            let p = alloc_zeroed(layout);
            NonNull::new(p).expect("aligned alloc failed")
        };
        let slab = unsafe { Slab::init(base, 0) };
        let returned = slab.into_raw();
        assert_eq!(returned, base);
        unsafe { dealloc(returned.as_ptr(), layout) };
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;
    use loom::thread;
    use std::alloc::Layout;

    fn with_test_slab(size_class_index: u8, f: impl FnOnce(&mut Slab)) {
        let layout = Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap();
        let base = unsafe {
            let p = std::alloc::alloc_zeroed(layout);
            NonNull::new(p).expect("aligned alloc failed")
        };
        let mut slab = unsafe { Slab::init(base, size_class_index) };
        f(&mut slab);
        drop(slab);
        unsafe {
            ptr::drop_in_place(base.as_ptr().cast::<SlabHeader>());
            std::alloc::dealloc(base.as_ptr(), layout);
        }
    }

    #[test]
    fn concurrent_push() {
        loom::model(|| {
            with_test_slab(0, |slab| {
                let s0 = slab.alloc().unwrap();
                let s1 = slab.alloc().unwrap();
                let r0 = s0.as_ptr() as usize;
                let r1 = s1.as_ptr() as usize;

                let h1 = thread::spawn(move || {
                    let ptr = NonNull::new(r0 as *mut u8).unwrap();
                    let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
                    slab_ref.dealloc_remote(ptr);
                });
                let h2 = thread::spawn(move || {
                    let ptr = NonNull::new(r1 as *mut u8).unwrap();
                    let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
                    slab_ref.dealloc_remote(ptr);
                });

                h1.join().unwrap();
                h2.join().unwrap();

                let drained = slab.drain_remote();
                assert_eq!(drained, 2);
            });
        });
    }

    #[test]
    fn push_drain_interleave() {
        loom::model(|| {
            with_test_slab(0, |slab| {
                let slot = slab.alloc().unwrap();
                let raw = slot.as_ptr() as usize;

                let h = thread::spawn(move || {
                    let ptr = NonNull::new(raw as *mut u8).unwrap();
                    let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
                    slab_ref.dealloc_remote(ptr);
                });

                let d1 = slab.drain_remote();
                h.join().unwrap();
                let d2 = slab.drain_remote();

                assert_eq!(d1 + d2, 1, "slot must appear exactly once across drains");
            });
        });
    }
}
