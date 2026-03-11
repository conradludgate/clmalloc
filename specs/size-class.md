# Size Classes

Size classes bucket allocation requests to reduce fragmentation and enable
O(1) free-list lookup from a Layout.

## Class selection

r[size-class.lookup]
Given a `Layout`, the allocator MUST compute the size class index in O(1)
time using arithmetic (not a lookup table scan).

r[size-class.round-up]
The size class MUST be the smallest class whose size is >= `layout.size()`
and whose alignment is >= `layout.align()`.

r[size-class.alignment]
Every size class size MUST be a multiple of the maximum alignment that class
serves. Classes serving alignment > 8 MUST have sizes that are multiples of
that alignment.

## Class definitions

r[size-class.small]
Size classes from 8 bytes up to 1024 bytes SHOULD use a spacing that wastes
at most 25% per allocation (e.g. powers of 2 with 4 intermediate steps,
giving a 1.25x growth factor).

r[size-class.medium]
Size classes from 1024 bytes up to 32768 bytes SHOULD use a spacing that
wastes at most 12.5% per allocation.

r[size-class.large]
Allocations larger than the maximum size class MUST be satisfied by
direct OS allocation (mmap) rather than slab allocation.

## Deallocation lookup

r[size-class.dealloc-index]
Given the `Layout` passed to `dealloc`, the allocator MUST recover the
same size class index that was used during `alloc` for that Layout.
