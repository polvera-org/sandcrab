//! Run ordering (bench-spec §4): ABAB interleaving at the iteration level, the starting
//! backend alternating per cell, and a seeded shuffle of the cell order.

use crate::rng::Rng;

/// Backend index for each of `n` iterations across `k` backends, ABAB style. Cell
/// `cell_index` starts at backend `cell_index % k`, so the leading backend alternates.
#[must_use]
pub fn interleave(k: usize, n: usize, cell_index: usize) -> Vec<usize> {
    if k == 0 {
        return Vec::new();
    }
    let start = cell_index % k;
    (0..n).map(|i| (start + i) % k).collect()
}

/// The cells in a seeded random order (the seed is recorded in the results).
#[must_use]
pub fn shuffled<T>(mut cells: Vec<T>, seed: u64) -> Vec<T> {
    Rng::new(seed ^ 0xCE11_0CDE).shuffle(&mut cells);
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abab_alternates_and_flips_per_cell() {
        assert_eq!(interleave(2, 4, 0), vec![0, 1, 0, 1]);
        assert_eq!(interleave(2, 4, 1), vec![1, 0, 1, 0]);
        assert_eq!(interleave(2, 3, 2), vec![0, 1, 0]);
        // a single available backend just runs back to back
        assert_eq!(interleave(1, 3, 5), vec![0, 0, 0]);
        assert!(interleave(0, 3, 0).is_empty());
    }

    #[test]
    fn abab_is_balanced() {
        for cell in 0..4 {
            let order = interleave(2, 30, cell);
            assert_eq!(order.iter().filter(|b| **b == 0).count(), 15);
            assert!(order.windows(2).all(|w| w[0] != w[1]), "no AA runs");
        }
    }

    #[test]
    fn shuffle_is_seeded() {
        let cells: Vec<u32> = (0..12).collect();
        let a = shuffled(cells.clone(), 99);
        assert_eq!(a, shuffled(cells.clone(), 99));
        assert_ne!(a, shuffled(cells.clone(), 100));
        let mut s = a.clone();
        s.sort_unstable();
        assert_eq!(s, cells);
    }
}
