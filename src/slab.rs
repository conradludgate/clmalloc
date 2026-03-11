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
pub(crate) const SLAB_MASK: usize = !(SLAB_SIZE - 1);

/// Opaque type representing a slab's memory region. Used as a typed pointer
/// target (`NonNull<SlabBase>`) instead of raw `NonNull<u8>` for slab bases.
#[repr(C, align(65536))]
pub struct SlabBase([u8; SLAB_SIZE]);

// -- Core unsafe primitives for intrusive free list --
//
// Every free slot stores a next-pointer in its first pointer-sized bytes.
// These two helpers are the only code that touches slot memory as free-list
// metadata. Unaligned access is used because not all size classes are
// pointer-aligned (e.g. 10-byte slots).

type Link = *mut u8;

#[inline]
unsafe fn read_next(slot: NonNull<u8>) -> Link {
    // SAFETY: Caller guarantees slot points to an allocated slot with at least
    // pointer-sized bytes for the free-list link.
    unsafe { slot.as_ptr().cast::<Link>().read_unaligned() }
}

#[inline]
pub(crate) unsafe fn write_next(slot: NonNull<u8>, next: Link) {
    // SAFETY: Caller guarantees slot points to an allocated slot with at least
    // pointer-sized bytes for the free-list link.
    unsafe { slot.as_ptr().cast::<Link>().write_unaligned(next) };
}

/// Intrusive singly-linked free list for slab slots.
///
/// Each free slot stores a next-pointer in its first pointer-sized bytes
/// (unaligned, since some size classes aren't pointer-aligned). Unsafe
/// is confined to `push`/`pop` which call `read_next`/`write_next`.
pub(crate) struct SlotFreeList {
    head: Link,
    len: u16,
}

impl SlotFreeList {
    pub const EMPTY: Self = Self {
        head: null_mut(),
        len: 0,
    };

    #[inline]
    pub fn pop(&mut self) -> Option<NonNull<u8>> {
        let head = NonNull::new(self.head)?;
        // SAFETY: head is from our free list, pointing to a valid slot
        // with at least pointer-sized bytes for the link.
        self.head = unsafe { read_next(head) };
        self.len -= 1;
        Some(head)
    }

    #[inline]
    pub fn push(&mut self, slot: NonNull<u8>) {
        // SAFETY: slot was allocated from a slab; has space for link.
        unsafe { write_next(slot, self.head) };
        self.head = slot.as_ptr();
        self.len += 1;
    }

    #[inline]
    pub fn len(&self) -> u16 {
        self.len
    }
}

/// Lock-free stack for cross-thread slot deallocation (Treiber stack).
///
/// Any thread can `push`; only the owning thread calls `swap_drain`.
/// Links are stored in the slot memory itself via `write_next`.
pub(crate) struct TreiberStack {
    head: AtomicPtr<u8>,
}

impl TreiberStack {
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(null_mut()),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Relaxed).is_null()
    }

    // r[impl slab.remote-freelist]
    /// Push a single slot via CAS loop.
    pub fn push(&self, slot: NonNull<u8>) {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: slot was allocated from a slab; has space for link.
            unsafe { write_next(slot, head) };
            match self.head.compare_exchange_weak(
                head,
                slot.as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }

    /// Push a pre-chained list `[first -> ... -> last]` in a single CAS.
    pub fn push_chain(&self, first: NonNull<u8>, last: NonNull<u8>) {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: last was allocated from a slab; has space for link.
            unsafe { write_next(last, head) };
            match self.head.compare_exchange_weak(
                head,
                first.as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }

    // r[impl slab.remote-drain]
    /// Atomically take the entire chain. Returns the raw head (null if empty).
    /// The caller walks the chain via `read_next`.
    pub fn swap_drain(&self) -> Link {
        self.head.swap(null_mut(), Ordering::Acquire)
    }
}

// r[impl slab.alignment] r[impl slab.single-class] r[impl slab.metadata] r[impl slab.owner]
#[repr(C)]
struct SlabHeader {
    slot_size: u16,
    slot_count: u16,
    slots_offset: u16,
    size_class_index: u8,
    // r[impl heap.identity]
    heap_id: usize,
    next_link: Option<NonNull<SlabBase>>,
    // r[impl slab.bump-alloc]
    /// Byte offset of the next slot to carve from the bump region.
    /// Once `bump_remaining == 0`, all slots have been carved and
    /// allocation falls back to the free list.
    bump_cursor: u16,
    bump_remaining: u16,
    // r[impl slab.local-freelist] r[impl slab.local-no-atomics]
    local: UnsafeCell<SlotFreeList>,
    // r[impl slab.remote-freelist]
    remote: TreiberStack,
}

impl SlabHeader {
    // r[impl slab.bump-alloc]
    #[allow(clippy::cast_possible_truncation)]
    unsafe fn init(base: NonNull<u8>, size_class_index: u8, heap_id: usize) -> NonNull<SlabHeader> {
        let slot_size = size_class::class_size(size_class_index as usize);
        debug_assert!(slot_size >= size_of::<Link>());

        let slots_offset = size_of::<Self>().next_multiple_of(slot_size);
        let slot_count = (SLAB_SIZE - slots_offset) / slot_size;
        debug_assert!(slot_count > 0 && u16::try_from(slot_count).is_ok());

        // SAFETY: base is a slab-aligned page from the pool; SlabHeader alignment is satisfied.
        #[expect(clippy::cast_ptr_alignment)]
        let header = base.as_ptr().cast::<SlabHeader>();
        // SAFETY: base is a valid, exclusively-owned slab page; slot_size/slot_count/class
        // are correct for the size class (validated by debug_assert above).
        unsafe {
            ptr::write(
                header,
                SlabHeader {
                    slot_size: slot_size as u16,
                    slot_count: slot_count as u16,
                    slots_offset: slots_offset as u16,
                    size_class_index,
                    heap_id,
                    next_link: None,
                    bump_cursor: slots_offset as u16,
                    bump_remaining: slot_count as u16,
                    local: UnsafeCell::new(SlotFreeList::EMPTY),
                    remote: TreiberStack::new(),
                },
            );
            NonNull::new_unchecked(header)
        }
    }

    unsafe fn from_ptr(ptr: *const u8) -> NonNull<SlabHeader> {
        // SAFETY: Caller guarantees ptr is within an allocated slab page; masking
        // yields the slab base which holds a valid SlabHeader.
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
    pub unsafe fn init(base: NonNull<SlabBase>, size_class_index: u8, heap_id: usize) -> Slab {
        // SAFETY: Caller guarantees base is valid, exclusively-owned slab memory.
        let header = unsafe { SlabHeader::init(base.cast(), size_class_index, heap_id) };
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

    /// Reconstruct an owner handle from a raw slab base pointer.
    ///
    /// # Safety
    /// - `base` must point to a previously initialized slab.
    /// - No other `Slab` handle may exist for this base.
    pub unsafe fn from_raw(base: NonNull<SlabBase>) -> Slab {
        Slab {
            header: base.cast(),
        }
    }

    /// Consume the owner handle, returning the raw slab base pointer.
    ///
    /// The caller (page pool) takes responsibility for the backing memory.
    /// The slab must be fully free (all slots returned) before calling this.
    pub fn into_raw(self) -> NonNull<SlabBase> {
        self.header.cast()
    }

    pub fn set_heap_id(&mut self, id: usize) {
        // SAFETY: Slab owns the header; exclusive access via &mut self.
        unsafe { (*self.header.as_ptr()).heap_id = id };
    }

    pub(crate) fn next_link(&self) -> Option<NonNull<SlabBase>> {
        self.header().next_link
    }

    pub(crate) fn set_next_link(&mut self, next: Option<NonNull<SlabBase>>) {
        // SAFETY: Slab owns the header; exclusive access via &mut self.
        unsafe { (*self.header.as_ptr()).next_link = next };
    }

    pub(crate) fn next_link_mut(&mut self) -> &mut Option<NonNull<SlabBase>> {
        // SAFETY: Slab owns the header; exclusive access via &mut self.
        unsafe { &mut (*self.header.as_ptr()).next_link }
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn slot_size(&self) -> usize {
        self.header().slot_size as usize
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn slot_count(&self) -> usize {
        self.header().slot_count as usize
    }

    pub fn size_class_index(&self) -> usize {
        self.header().size_class_index as usize
    }

    /// Available slots (free list + uncarved bump region). Does not include
    /// remotely freed slots — call `drain_remote` first for a complete count.
    pub fn free_count(&self) -> u16 {
        let header = self.header();
        // SAFETY: with_mut provides exclusive access to SlotFreeList.
        header.local.with_mut(|p| unsafe { (*p).len() }) + header.bump_remaining
    }

    // r[impl slab.return-to-pool]
    /// True when every slot is available (none outstanding).
    /// Call `drain_remote` first to account for remotely freed slots.
    pub fn is_fully_free(&self) -> bool {
        self.free_count() == self.header().slot_count
    }

    // r[impl slab.local-freelist] r[impl slab.local-no-atomics] r[impl slab.bump-alloc]
    /// Allocate a slot. Tries the free list first (recycled slots stay
    /// cache-hot), then falls back to the bump pointer for virgin slots.
    #[inline]
    pub fn alloc(&mut self) -> Option<NonNull<u8>> {
        // SAFETY: Slab owns the header; exclusive access via &mut self.
        let header = unsafe { &mut *self.header.as_ptr() };

        // Fast path: pop from the local free list (recycled slots).
        let ptr = header.local.with_mut(|p| {
            // SAFETY: with_mut provides exclusive access to SlotFreeList.
            unsafe { &mut *p }.pop()
        });
        if ptr.is_some() {
            return ptr;
        }

        // Bump-pointer path: carve a fresh slot (no memory reads needed).
        if header.bump_remaining > 0 {
            let base = self.header.as_ptr().cast::<u8>();
            // SAFETY: bump_cursor is within [slots_offset, SLAB_SIZE); base + offset is in slab.
            let slot = unsafe { base.add(header.bump_cursor as usize) };
            header.bump_cursor = header.bump_cursor.wrapping_add(header.slot_size);
            header.bump_remaining -= 1;
            // SAFETY: slot is within the slab's bump region, non-null.
            return Some(unsafe { NonNull::new_unchecked(slot) });
        }

        None
    }

    // r[impl slab.local-freelist] r[impl slab.local-no-atomics]
    /// Return a slot to the local free list. O(1).
    #[inline]
    pub fn dealloc_local(&mut self, ptr: NonNull<u8>) {
        self.header().local.with_mut(|p| {
            // SAFETY: with_mut provides exclusive access to SlotFreeList.
            unsafe { &mut *p }.push(ptr);
        });
    }

    /// True if the remote free list has pending entries. Cheaper than
    /// `drain_remote` when we only need to know whether draining is worthwhile.
    pub fn has_pending_remote(&self) -> bool {
        !self.header().remote.is_empty()
    }

    // r[impl slab.remote-drain]
    /// Atomically drain the remote free list into the local free list.
    ///
    /// Swaps the remote head to null, walks the chain, and prepends each
    /// node to the local list. Returns the number of slots recovered.
    pub fn drain_remote(&mut self) -> u16 {
        let header = self.header();
        let chain = header.remote.swap_drain();
        header.local.with_mut(|p| {
            // SAFETY: with_mut provides exclusive access to SlotFreeList.
            let free_list = unsafe { &mut *p };
            let mut count = 0u16;
            let mut cursor = chain;
            while let Some(slot) = NonNull::new(cursor) {
                // SAFETY: slot is from remote list, valid allocated slot.
                // Read next before push overwrites the link.
                let next = unsafe { read_next(slot) };
                free_list.push(slot);
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
    #[inline]
    pub unsafe fn from_interior_ptr(ptr: *const u8) -> SlabRef {
        // SAFETY: Caller guarantees ptr is within a live, initialized slab.
        SlabRef {
            header: unsafe { SlabHeader::from_ptr(ptr) },
        }
    }

    #[inline]
    pub fn heap_id(self) -> usize {
        self.header().heap_id
    }

    /// True if two `SlabRef`s point to the same slab header.
    #[inline]
    pub fn header_eq(self, other: SlabRef) -> bool {
        self.header == other.header
    }

    #[expect(dead_code)]
    pub fn slot_size(self) -> usize {
        self.header().slot_size as usize
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn slot_count(self) -> usize {
        self.header().slot_count as usize
    }

    #[expect(dead_code)]
    pub fn size_class_index(self) -> usize {
        self.header().size_class_index as usize
    }

    // r[impl slab.remote-freelist]
    /// Push a freed slot onto the remote free list via atomic CAS.
    pub fn dealloc_remote(self, ptr: NonNull<u8>) {
        self.header().remote.push(ptr);
    }

    /// Push a pre-chained list `[first -> ... -> last]` onto the remote
    /// free list in a single CAS. Used by the free cache flush to batch
    /// multiple frees into one atomic operation per slab.
    pub fn push_chain_remote(self, first: NonNull<u8>, last: NonNull<u8>) {
        self.header().remote.push_chain(first, last);
    }
}

// ---------------------------------------------------------------------------
// SlabList — intrusive singly-linked list of slabs via next_link
// ---------------------------------------------------------------------------

pub(crate) type SlabList = Option<NonNull<SlabBase>>;

/// Push a slab onto the head of a slab list, consuming the owner handle.
pub(crate) fn slab_list_push(head: &mut SlabList, mut slab: Slab) {
    slab.set_next_link(*head);
    *head = Some(slab.into_raw());
}

/// Pop a slab from the head of a slab list, returning the owner handle.
pub(crate) fn slab_list_pop(head: &mut SlabList) -> Option<Slab> {
    let base = (*head)?;
    // SAFETY: base was put into the list via slab_list_push / into_raw.
    let mut slab = unsafe { Slab::from_raw(base) };
    *head = slab.next_link();
    slab.set_next_link(None);
    Some(slab)
}

/// Cursor for in-place traversal and removal on a slab chain.
///
/// # Safety
///
/// The raw pointer `prev` must remain valid and unaliased for the
/// cursor's lifetime. The caller must not access the list head through
/// any other path while the cursor is live.
pub(crate) struct SlabListCursor {
    prev: *mut SlabList,
}

impl SlabListCursor {
    /// # Safety
    ///
    /// `head` must point to a valid `SlabList` that remains live and
    /// unaliased for the lifetime of the cursor.
    pub unsafe fn new(head: *mut SlabList) -> Self {
        Self { prev: head }
    }

    pub fn current(&self) -> Option<NonNull<SlabBase>> {
        // SAFETY: prev is valid per construction invariant.
        unsafe { *self.prev }
    }

    /// Remove the current node from the list and advance to its successor.
    /// `slab` must be the owner handle for the current node.
    pub fn remove_current(&mut self, slab: &Slab) {
        let next = slab.next_link();
        // SAFETY: prev is valid per construction invariant.
        unsafe { *self.prev = next };
    }

    /// Advance past the current node, keeping it in the list.
    /// `slab` must be the owner handle for the current node.
    pub fn advance(&mut self, slab: &mut Slab) {
        self.prev = slab.next_link_mut();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::collections::HashSet;

    struct TestSlab {
        slab: Slab,
        ptr: NonNull<SlabBase>,
        layout: Layout,
    }

    impl TestSlab {
        fn new(size_class_index: u8) -> Self {
            let layout = Layout::from_size_align(SLAB_SIZE, SLAB_SIZE).unwrap();
            let ptr = unsafe {
                // SAFETY: layout has valid size/align; alloc_zeroed returns valid or null.
                let p = alloc_zeroed(layout);
                NonNull::new(p)
                    .expect("aligned alloc failed")
                    .cast::<SlabBase>()
            };
            // SAFETY: ptr is SLAB_SIZE-aligned, valid for SLAB_SIZE bytes.
            let slab = unsafe { Slab::init(ptr, size_class_index, 0) };
            Self { slab, ptr, layout }
        }
    }

    impl Drop for TestSlab {
        fn drop(&mut self) {
            // SAFETY: ptr and layout match the original alloc_zeroed call.
            unsafe { dealloc(self.ptr.as_ptr().cast(), self.layout) }
        }
    }

    // r[verify slab.alignment]
    #[test]
    fn interior_ptr_recovers_header() {
        let mut t = TestSlab::new(0);
        let slot0 = t.slab.alloc().unwrap();
        let slot1 = t.slab.alloc().unwrap();

        // SAFETY: slot0/slot1 are from t.slab.alloc(), interior to the slab.
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

    #[test]
    fn slab_is_send_slabref_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send::<Slab>();
        assert_send_sync::<SlabRef>();
    }

    // r[verify slab.local-freelist] r[verify slab.bump-alloc]
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
        assert_eq!(drained, u16::try_from(n).unwrap());

        let mut recovered = HashSet::new();
        for _ in 0..n {
            recovered.insert(t.slab.alloc().unwrap().as_ptr() as usize);
        }
        let originals: HashSet<usize> = raws.into_iter().collect();
        assert_eq!(recovered, originals);
    }

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
            // SAFETY: layout has valid size/align; alloc_zeroed returns valid or null.
            let p = alloc_zeroed(layout);
            NonNull::new(p)
                .expect("aligned alloc failed")
                .cast::<SlabBase>()
        };
        // SAFETY: base is SLAB_SIZE-aligned, valid for SLAB_SIZE bytes.
        let slab = unsafe { Slab::init(base, 0, 0) };
        let returned = slab.into_raw();
        assert_eq!(returned, base);
        // SAFETY: returned and layout match the original alloc.
        unsafe { dealloc(returned.as_ptr().cast(), layout) };
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
            // SAFETY: layout has valid size/align; alloc_zeroed returns valid or null.
            let p = std::alloc::alloc_zeroed(layout);
            NonNull::new(p)
                .expect("aligned alloc failed")
                .cast::<SlabBase>()
        };
        // SAFETY: base is SLAB_SIZE-aligned, valid for SLAB_SIZE bytes.
        let mut slab = unsafe { Slab::init(base, size_class_index, 0) };
        f(&mut slab);
        drop(slab);
        unsafe {
            // SAFETY: base was initialized as SlabHeader; we manually drop before dealloc.
            ptr::drop_in_place(base.as_ptr().cast::<SlabHeader>());
            std::alloc::dealloc(base.as_ptr().cast(), layout);
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
                    // SAFETY: ptr is from slab.alloc(), interior to the slab.
                    let slab_ref = unsafe { SlabRef::from_interior_ptr(ptr.as_ptr()) };
                    slab_ref.dealloc_remote(ptr);
                });
                let h2 = thread::spawn(move || {
                    let ptr = NonNull::new(r1 as *mut u8).unwrap();
                    // SAFETY: ptr is from slab.alloc(), interior to the slab.
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
                    // SAFETY: ptr is from slab.alloc(), interior to the slab.
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
