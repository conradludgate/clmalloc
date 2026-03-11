use core::alloc::Layout;

/// Minimum allocation size (and minimum guaranteed alignment).
const MIN_SIZE: usize = 8;
/// Sub-steps per power-of-2 group (gives <= 25% waste).
const SUB_STEPS: usize = 4;
/// Smallest group exponent (2^3 = 8).
const MIN_K: usize = 3;
/// Largest regular group exponent (2^14 = 16384).
const MAX_K: usize = 14;
const NUM_GROUPS: usize = MAX_K - MIN_K + 1;
const NUM_REGULAR: usize = NUM_GROUPS * SUB_STEPS;
const MAX_SLAB_SIZE: usize = 32768;

/// Total number of slab-served size classes.
pub const NUM_CLASSES: usize = NUM_REGULAR + 1;

// r[impl size-class.lookup] r[impl size-class.round-up] r[impl size-class.alignment] r[impl size-class.dealloc-index]
/// Returns the size class index for a layout, or `None` for large allocations.
///
/// O(1) via bit arithmetic. Rounds size up to the next multiple of `align`,
/// then finds the smallest size class that fits.
#[inline]
pub fn class_index(layout: Layout) -> Option<usize> {
    let size = layout.size().next_multiple_of(layout.align()).max(MIN_SIZE);

    if size > MAX_SLAB_SIZE {
        return None;
    }
    if size == MAX_SLAB_SIZE {
        return Some(NUM_REGULAR);
    }

    let k = (usize::BITS - 1 - size.leading_zeros()) as usize;
    let base = 1usize << k;
    let step = base >> 2;
    let j = (size - base).div_ceil(step);

    if j >= SUB_STEPS {
        let next_k = k + 1;
        if next_k > MAX_K {
            return Some(NUM_REGULAR);
        }
        return Some((next_k - MIN_K) * SUB_STEPS);
    }

    Some((k - MIN_K) * SUB_STEPS + j)
}

// r[impl size-class.small] r[impl size-class.medium] r[impl size-class.large]
/// Returns the allocation size for a size class index.
pub fn class_size(index: usize) -> usize {
    debug_assert!(index < NUM_CLASSES);

    if index >= NUM_REGULAR {
        return MAX_SLAB_SIZE;
    }

    let group = index / SUB_STEPS;
    let sub = index % SUB_STEPS;
    let k = group + MIN_K;
    let base = 1usize << k;
    let step = base >> 2;
    base + sub * step
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;

    // r[verify size-class.round-up]
    #[test]
    fn class_is_smallest_fit() {
        for size in 1..=MAX_SLAB_SIZE {
            let layout = Layout::from_size_align(size, 1).unwrap();
            let idx = class_index(layout).unwrap();
            let cs = class_size(idx);
            assert!(cs >= size, "class {idx} size {cs} < requested {size}");
            if idx > 0 {
                let prev = class_size(idx - 1);
                assert!(
                    prev < size,
                    "previous class {prev} fits {size}, should have picked it"
                );
            }
        }
    }

    // r[verify size-class.alignment]
    #[test]
    fn class_sizes_respect_alignment() {
        for idx in 0..NUM_CLASSES {
            let cs = class_size(idx);
            let max_align = 1usize << cs.trailing_zeros();
            assert!(
                cs.is_multiple_of(max_align),
                "class {idx} size {cs} not a multiple of its alignment {max_align}"
            );
        }
    }

    // r[verify size-class.alignment]
    #[test]
    fn alignment_requests_get_aligned_class() {
        for align_exp in 0..=12 {
            let align = 1usize << align_exp;
            for size in 1..=MAX_SLAB_SIZE {
                let layout = match Layout::from_size_align(size, align) {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                if let Some(idx) = class_index(layout) {
                    let cs = class_size(idx);
                    assert!(
                        cs.is_multiple_of(align),
                        "size={size} align={align} -> class {idx} size {cs} not aligned"
                    );
                }
            }
        }
    }

    // r[verify size-class.small]
    #[test]
    fn small_class_waste_within_25_percent() {
        for size in 8..=1024 {
            let layout = Layout::from_size_align(size, 1).unwrap();
            let idx = class_index(layout).unwrap();
            let cs = class_size(idx);
            let waste_pct = ((cs - size) as f64 / size as f64) * 100.0;
            assert!(
                waste_pct <= 25.0,
                "size {size} -> class size {cs}, waste {waste_pct:.1}% > 25%"
            );
        }
    }

    // r[verify size-class.medium]
    #[test]
    fn medium_class_waste_within_25_percent() {
        for size in 1025..=MAX_SLAB_SIZE {
            let layout = Layout::from_size_align(size, 1).unwrap();
            let idx = class_index(layout).unwrap();
            let cs = class_size(idx);
            let waste_pct = ((cs - size) as f64 / size as f64) * 100.0;
            assert!(
                waste_pct <= 25.0,
                "size {size} -> class size {cs}, waste {waste_pct:.1}% > 25%"
            );
        }
    }

    // r[verify size-class.large]
    #[test]
    fn large_allocations_return_none() {
        for size in [MAX_SLAB_SIZE + 1, 65536, 1 << 20] {
            let layout = Layout::from_size_align(size, 1).unwrap();
            assert!(class_index(layout).is_none(), "size {size} should be large");
        }
    }

    // r[verify size-class.dealloc-index]
    #[test]
    fn dealloc_round_trip() {
        for size in 1..=MAX_SLAB_SIZE {
            let layout = Layout::from_size_align(size, 1).unwrap();
            let idx = class_index(layout).unwrap();
            let cs = class_size(idx);
            let round_trip_layout = Layout::from_size_align(cs, 1).unwrap();
            let idx2 = class_index(round_trip_layout).unwrap();
            assert_eq!(
                idx, idx2,
                "round-trip failed for size {size}: {idx} != {idx2}"
            );
        }
    }

    // r[verify size-class.lookup]
    #[test]
    fn class_sizes_are_monotonic() {
        for i in 1..NUM_CLASSES {
            assert!(
                class_size(i) > class_size(i - 1),
                "class sizes not monotonic at {i}: {} <= {}",
                class_size(i),
                class_size(i - 1)
            );
        }
    }
}
