//! Populated-range bookkeeping for the disk cache.
//!
//! Tracks which byte ranges of a sparse cache file have been written to.
//! Half-open `[start, end)` intervals stored in a `BTreeMap<start, end>`.
//! Adjacent and overlapping intervals are merged on insert.

use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct PopulatedRanges {
    intervals: BTreeMap<u64, u64>,
}

impl PopulatedRanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `[start, end)`. Merges with overlapping/adjacent intervals.
    pub fn insert(&mut self, mut start: u64, mut end: u64) {
        if start >= end {
            return;
        }

        // Find any interval that ends at or after `start` and starts at or
        // before `end`. Merge them all into one.
        let to_merge: Vec<u64> = self
            .intervals
            .range(..=end)
            .filter(|(_, &iend)| iend >= start)
            .map(|(&istart, _)| istart)
            .collect();

        for istart in to_merge {
            let iend = self
                .intervals
                .remove(&istart)
                .expect("just collected start");
            start = start.min(istart);
            end = end.max(iend);
        }
        self.intervals.insert(start, end);
    }

    /// True iff `[start, end)` is fully populated.
    pub fn contains_range(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        // Find the interval whose start is <= our start.
        match self.intervals.range(..=start).next_back() {
            Some((_, &iend)) => iend >= end,
            None => false,
        }
    }

    /// First missing sub-range within `[start, end)`, or `None` if fully populated.
    pub fn first_gap(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        if start >= end {
            return None;
        }
        // Walk the gap forward from `start`.
        let mut cursor = start;
        for (&istart, &iend) in self.intervals.range(..end) {
            if iend <= cursor {
                continue;
            }
            if istart > cursor {
                return Some((cursor, istart.min(end)));
            }
            // istart <= cursor && iend > cursor → cursor is inside this interval
            cursor = iend;
            if cursor >= end {
                return None;
            }
        }
        if cursor < end {
            Some((cursor, end))
        } else {
            None
        }
    }

    /// Total bytes populated across all intervals.
    pub fn total_bytes(&self) -> u64 {
        self.intervals.iter().map(|(s, e)| e - s).sum()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_contains_nothing() {
        let r = PopulatedRanges::new();
        assert!(!r.contains_range(0, 100));
        assert!(r.contains_range(0, 0)); // empty range trivially contained
        assert_eq!(r.first_gap(0, 100), Some((0, 100)));
    }

    #[test]
    fn insert_single_range() {
        let mut r = PopulatedRanges::new();
        r.insert(10, 20);
        assert!(r.contains_range(10, 20));
        assert!(r.contains_range(15, 18));
        assert!(!r.contains_range(0, 10));
        assert!(!r.contains_range(20, 30));
        assert!(!r.contains_range(15, 25));
    }

    #[test]
    fn insert_merges_overlapping() {
        let mut r = PopulatedRanges::new();
        r.insert(10, 20);
        r.insert(15, 30);
        assert!(r.contains_range(10, 30));
        assert_eq!(r.intervals.len(), 1);
    }

    #[test]
    fn insert_merges_adjacent() {
        let mut r = PopulatedRanges::new();
        r.insert(10, 20);
        r.insert(20, 30); // touching boundary
        assert!(r.contains_range(10, 30));
        assert_eq!(r.intervals.len(), 1);
    }

    #[test]
    fn insert_keeps_disjoint_separate() {
        let mut r = PopulatedRanges::new();
        r.insert(10, 20);
        r.insert(30, 40);
        assert_eq!(r.intervals.len(), 2);
        assert!(!r.contains_range(20, 30));
    }

    #[test]
    fn insert_merges_chain() {
        // 3 disjoint intervals, then a 4th that bridges them all
        let mut r = PopulatedRanges::new();
        r.insert(10, 20);
        r.insert(30, 40);
        r.insert(50, 60);
        assert_eq!(r.intervals.len(), 3);
        r.insert(15, 55);
        assert_eq!(r.intervals.len(), 1);
        assert!(r.contains_range(10, 60));
    }

    #[test]
    fn first_gap_at_start() {
        let mut r = PopulatedRanges::new();
        r.insert(50, 100);
        assert_eq!(r.first_gap(0, 100), Some((0, 50)));
    }

    #[test]
    fn first_gap_in_middle() {
        let mut r = PopulatedRanges::new();
        r.insert(0, 30);
        r.insert(50, 100);
        assert_eq!(r.first_gap(0, 100), Some((30, 50)));
    }

    #[test]
    fn first_gap_at_end() {
        let mut r = PopulatedRanges::new();
        r.insert(0, 50);
        assert_eq!(r.first_gap(0, 100), Some((50, 100)));
    }

    #[test]
    fn first_gap_none_when_fully_covered() {
        let mut r = PopulatedRanges::new();
        r.insert(0, 100);
        assert_eq!(r.first_gap(0, 100), None);
        assert_eq!(r.first_gap(20, 80), None);
    }

    #[test]
    fn total_bytes_sums_intervals() {
        let mut r = PopulatedRanges::new();
        r.insert(0, 10);
        r.insert(20, 30);
        assert_eq!(r.total_bytes(), 20);
        r.insert(5, 25); // merges into (0, 30)
        assert_eq!(r.total_bytes(), 30);
    }

    #[test]
    fn empty_range_insert_is_noop() {
        let mut r = PopulatedRanges::new();
        r.insert(10, 10);
        r.insert(20, 5); // start > end
        assert!(r.is_empty());
    }
}
