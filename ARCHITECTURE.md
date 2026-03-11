# clmalloc Architecture

A thread-caching slab allocator inspired by mimalloc's free-list sharding.

## Layer Overview

```
 ┌─────────────────────────────────────────────────────────┐
 │                     Application                         │
 │              alloc(layout) / dealloc(ptr)                │
 └───────────────────────┬─────────────────────────────────┘
                         │
 ┌───────────────────────▼─────────────────────────────────┐
 │                  GlobalAlloc (global.rs)                 │
 │                                                         │
 │  ┌──────────┐   pthread_getspecific   ┌──────────────┐  │
 │  │ ZST gate ├────────────────────────►│ Thread Heap  │  │
 │  └──────────┘                         └──────┬───────┘  │
 │       │                                      │          │
 │       │  sentinel / null                     │          │
 │       ▼                                      │          │
 │  ┌───────────────────┐                       │          │
 │  │ post-exit fallback│                       │          │
 │  │ SlabRef::dealloc_ │                       │          │
 │  │ remote() / munmap │                       │          │
 │  └───────────────────┘                       │          │
 └──────────────────────────────────────────────┼──────────┘
                                                │
 ┌──────────────────────────────────────────────▼──────────┐
 │                 Thread-Local Heap (heap.rs)              │
 │                                                         │
 │  Per size class (×49):                                   │
 │  ┌────────┐  ┌──────────────┐  ┌──────────┐            │
 │  │ Cache  │  │ Active Slab  │  │  Page     │            │
 │  │(64 ent)│  │  (bins[i])   │  │  Queues   │            │
 │  └───┬────┘  └──────┬───────┘  └──┬───────┘            │
 │      │              │             │                     │
 │      │  pop first   │ pop/bump    │ partial → active    │
 │      ▼              ▼             ▼                     │
 │   ┌─────────────────────────────────────────┐           │
 │   │  alloc: cache → slab.local → slab.bump  │           │
 │   │         → partial queue → scan full list │           │
 │   │         → adopt abandoned → new slab     │           │
 │   └─────────────────────────────────────────┘           │
 └─────────────────────────────────────────────────────────┘
                         │
                    (cold path)
                         │
 ┌───────────────────────▼─────────────────────────────────┐
 │              Global Page Pool (pool.rs)                  │
 │                         spin::Mutex                      │
 │                                                         │
 │  ┌──────────┐  ┌──────────────┐  ┌───────────────────┐ │
 │  │Free list │  │Segment carve │  │ Abandoned lists   │ │
 │  │(recycled │  │(bump through │  │ (per size class)  │ │
 │  │ slabs)   │  │ 2MiB region) │  │                   │ │
 │  └──────────┘  └──────────────┘  └───────────────────┘ │
 │                                                         │
 │  ┌─────────────────────────────────────────────────┐    │
 │  │ Segment tracking: seg_outstanding[] per segment │    │
 │  │ When outstanding hits 0 → purge (munmap)        │    │
 │  └─────────────────────────────────────────────────┘    │
 └─────────────────────────────────────────────────────────┘
                         │
                    (mmap outside lock)
                         │
 ┌───────────────────────▼─────────────────────────────────┐
 │            PageAllocator trait (sys.rs)                  │
 │                                                         │
 │   MmapAllocator (production)  SystemAllocator (tests)   │
 │   mmap MAP_ANON|MAP_PRIVATE   std::alloc::alloc_zeroed  │
 └─────────────────────────────────────────────────────────┘
```

## Allocation Fast Path

```
  alloc(layout)
      │
      ▼
  ┌─ size == 0 ? ──yes──► return dangling ptr
  │
  ▼
  class_index(layout) ──None──► pool.alloc_large(layout)
      │
      │ Some(idx)
      ▼
  cache[idx].pop() ────hit────► return ptr       ◄── hottest path
      │
      │ miss
      ▼
  active slab local free list
  slab.alloc() ────────hit────► return ptr
      │
      │ empty
      ▼
  slab.bump_remaining > 0 ?
      │yes                │no
      ▼                   ▼
  bump-alloc slot    drain remote free list
  return ptr         slab.alloc() ─hit─► return ptr
                          │
                          │ still empty
                          ▼
                     retire active → full list
                     flush cache (sorts by slab base,
                       batch CAS per remote slab)
                          │
                          ▼
                     pop partial list ─hit─► install as active, retry
                          │
                          │ empty
                          ▼
                     scan full list (drain remote on each):
                       ├─ fully free → return to pool
                       ├─ has free slots → active or partial list
                       └─ still full → leave on full list
                          │
                          ▼
                     pop partial list ─hit─► retry
                          │
                          │ empty
                          ▼
                     try_adopt (abandoned list):
                       eagerly adopt ALL for this class
                       ├─ fully free → return to pool
                       ├─ allocable → install as active
                       └─ remaining → partial or full list
                          │
                          │ none available
                          ▼
                     pool.alloc_slab() → init → install as active
```

## Deallocation Dispatch

```
  dealloc(ptr, layout)
      │
      ▼
  ┌─ size == 0 ? ──yes──► return (no-op)
  │
  ▼
  ┌─ heap destroyed ? ──yes──► post-exit fallback:
  │   (sentinel/null)          class_index? → SlabRef::dealloc_remote
  │                                     None → pool.dealloc_large
  ▼
  class_index(layout) ──None──► pool.dealloc_large(ptr)
      │
      │ Some(idx)
      ▼
  ptr in active slab ?
      │yes                       │no
      ▼                          ▼
  slab.dealloc_local(ptr)   cache[idx].push(ptr)     ◄── O(1), no atomics
  (local free list, O(1),        │
   no atomics)                   │ cache full?
                                 │yes
                                 ▼
                            flush_cache(idx):
                              sort entries by slab base
                              for each slab run:
                                ├─ own heap_id → slab.dealloc_local each
                                └─ remote      → chain entries, 1× CAS push
```

## Slab Internal Structure (64 KB, aligned to 64 KB)

```
  0x????_0000  ┌─────────────────────────────────────────┐
               │              SlabHeader                  │
               │  ┌─────────────────────────────────────┐ │
               │  │ slot_size: u16                      │ │
               │  │ slot_count: u16                     │ │
               │  │ slots_offset: u16                   │ │
               │  │ size_class_index: u8                │ │
               │  │ heap_id: usize     ◄── ownership    │ │
               │  │ next_link: Option<NonNull<SlabBase>> │ │
               │  │ bump_cursor: u16                    │ │
               │  │ bump_remaining: u16                 │ │
               │  │                                     │ │
               │  │ local: UnsafeCell<LocalState>       │ │
               │  │   ├─ head: *mut u8  ◄── free list   │ │
               │  │   └─ free_count: u16                │ │
               │  │                                     │ │
               │  │ remote_head: AtomicPtr<u8>          │ │
               │  │   └─ Treiber stack (CAS push,       │ │
               │  │      atomic-swap drain)              │ │
               │  └─────────────────────────────────────┘ │
               ├──────────────────────────────────────────┤
               │  padding (to slot_size alignment)        │
  slots_offset ├──────────────────────────────────────────┤
               │  Slot 0  [next-ptr | user data]          │◄─ bump cursor
               ├──────────────────────────────────────────┤   starts here
               │  Slot 1  [next-ptr | user data]          │
               ├──────────────────────────────────────────┤
               │  Slot 2  ...                             │
               │  ...                                     │
               ├──────────────────────────────────────────┤
               │  Slot N-1                                │
  0x????_FFFF  └──────────────────────────────────────────┘

  Handle types:

    Slab (&mut self)              SlabRef (Copy, Send+Sync)
    ┌────────────────────┐        ┌────────────────────┐
    │ Owner handle       │        │ Shared handle      │
    │ • alloc()          │        │ • dealloc_remote() │
    │ • dealloc_local()  │        │ • push_chain_      │
    │ • drain_remote()   │        │     remote()       │
    │ • is_fully_free()  │        │ • heap_id()        │
    │                    │        │ • from_interior_   │
    │ Send + !Sync       │        │     ptr()          │
    └────────────────────┘        │ Send + Sync        │
                                  └────────────────────┘

  Pointer → Slab lookup:  slab_base = ptr & 0xFFFF_FFFF_FFFF_0000
```

## Page Pool and Segment Management

```
  ┌─────────── Segment (2 MiB, mmap'd) ───────────────────┐
  │                                                        │
  │  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐     ┌──────┐   │
  │  │Slab 0│ │Slab 1│ │Slab 2│ │Slab 3│ ... │Slab31│   │
  │  │ 64KB │ │ 64KB │ │ 64KB │ │ 64KB │     │ 64KB │   │
  │  └──────┘ └──────┘ └──────┘ └──────┘     └──────┘   │
  │                         ▲                             │
  │                    segment_cursor                      │
  │                    (bump pointer)                      │
  └────────────────────────────────────────────────────────┘

  Pool state (under spin::Mutex):

    free_head ──► [slab]──► [slab]──► [slab]──► null
                  (intrusive linked list through slab memory)

    segments[0..N]:     pointers to each mmap'd segment
    seg_outstanding[i]: count of in-use slabs per segment

    abandoned_heads[0..49]: per-class singly-linked slab lists

  Allocation:
    1. Pop free list           (lock held, fast)
    2. Bump from segment       (lock held, fast)
    3. mmap new segment        (lock RELEASED, then re-acquire)

  Purge:
    When seg_outstanding[i] drops to 0 and segment fully carved:
      → unlink all segment's slabs from free list
      → swap-remove segment from tracking arrays
      → munmap the 2 MiB region (lock released first)
```

## Heap Page Queues (per size class)

```
  ┌───────────────────── Size class i ─────────────────────┐
  │                                                        │
  │  bins[i]: Option<Slab>                                 │
  │  ┌─────────────────┐                                   │
  │  │  Active Slab     │  ◄── alloc/dealloc_local happen  │
  │  │  (owned, &mut)   │      here without atomics        │
  │  └─────────────────┘                                   │
  │                                                        │
  │  partial_heads[i]:        full_heads[i]:               │
  │  ┌──────┐ ┌──────┐       ┌──────┐ ┌──────┐            │
  │  │Slab A│→│Slab B│→null  │Slab X│→│Slab Y│→null       │
  │  │has   │ │has   │       │0 free│ │0 free│             │
  │  │free  │ │free  │       │slots │ │slots │             │
  │  │slots │ │slots │       │(await│ │(await│             │
  │  └──────┘ └──────┘       │remote│ │remote│             │
  │                          │frees)│ │frees)│             │
  │                          └──────┘ └──────┘             │
  │                                                        │
  │  caches[i]: FreeCache                                  │
  │  ┌─────────────────────────────────────────┐           │
  │  │ [ptr, ptr, ptr, ..., ptr]  (up to 64)   │           │
  │  │  absorbs non-active-slab frees          │           │
  │  │  popped before active slab on alloc     │           │
  │  └─────────────────────────────────────────┘           │
  └────────────────────────────────────────────────────────┘

  Transitions:

    active ──(exhausted)──► full list
    full   ──(scan+drain, has free)──► active or partial
    full   ──(scan+drain, all free)──► pool.dealloc_slab
    partial──(pop)──► active
```

## Thread Lifecycle

```
  Thread start
      │
      ▼
  First allocation
      │
      ▼
  pthread_getspecific → null
      │
      ▼
  libc::malloc(sizeof Heap)    ◄── allocate heap via libc,
  Heap::new(&pool)                 not through clmalloc
  pthread_setspecific(key, ptr)
      │
      ▼
  ┌────────────────────────────────────────────┐
  │         Normal operation                    │
  │  alloc/dealloc through thread-local heap    │
  │  no locks on fast path                      │
  └────────────────────────────────────────────┘
      │
      ▼
  Thread exit → pthread destructor fires
      │
      ▼
  Heap::drop():
    for each size class:
      1. flush_cache → return cached ptrs to owning slabs
      2. retire active slab:
           fully free → pool.dealloc_slab
           in use    → pool.abandon_slab
      3. drain full + partial lists:
           fully free → pool.dealloc_slab
           in use    → pool.abandon_slab
      │
      ▼
  libc::free(heap_ptr)
  pthread_setspecific(key, SENTINEL)
      │
      ▼
  Post-exit deallocs from other threads:
    get_heap() returns SENTINEL
    → SlabRef::dealloc_remote (direct CAS)
    → pool.dealloc_large (for large allocs)
```

## Size Class Map

```
  49 classes total: 48 regular + 1 max (32 KB)

  Group k (2^k base, 4 sub-steps each):

  k=3:   8   10   12   14           (8B group)
  k=4:  16   20   24   28           (16B group)
  k=5:  32   40   48   56           (32B group)
  k=6:  64   80   96  112           (64B group)
  k=7: 128  160  192  224           (128B group)
  ...
  k=14: 16384 20480 24576 28672     (16KB group)
  +1:   32768                       (max slab class)

  > 32768: large allocation (direct mmap, no slab)

  Growth factor: 1.25× between adjacent classes
  Max waste: ≤ 25% per allocation

  Lookup: O(1) via bit arithmetic (leading_zeros + division)
  Round-trip: class_index(alloc_layout) == class_index(dealloc_layout)
```
