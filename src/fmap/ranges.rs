//! Map of disjoint `u64` ranges to values that split and merge.
//!
//! Replaces the `range-tree` crate the Redox scheme used, whose `remove`
//! re-inserts phantom pieces when the removed range is wider than a stored
//! one and skips pieces that start exactly where the previous one ended.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Range;

/// A value covering a range, which can be split into parts and merged with
/// adjacent values.
pub trait Value: Sized {
    /// Merge `self` with `other`, which ends where `self` starts.
    fn try_merge_backwards(self, other: &Self) -> Result<Self, Self>;
    /// Merge `self` with `other`, which starts where `self` ends.
    fn try_merge_forward(self, other: &Self) -> Result<Self, Self>;
    /// Split into values for `prev_range`, `range` and `next_range`, which
    /// partition the range `self` covered.
    fn split(
        self,
        prev_range: Option<Range<u64>>,
        range: Range<u64>,
        next_range: Option<Range<u64>>,
    ) -> (Option<Self>, Self, Option<Self>);
}

#[derive(Debug)]
pub struct Occupied;

#[derive(Debug)]
pub struct RangeMap<V> {
    /// Start of each range to its end and value.
    inner: BTreeMap<u64, (u64, V)>,
}

impl<V> Default for RangeMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> RangeMap<V> {
    pub const fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
        }
    }

    /// Start of the first stored range overlapping `range`, if any lies at
    /// or before `range.start`.
    fn first_start(&self, range: &Range<u64>) -> u64 {
        self.inner
            .range(..range.start)
            .next_back()
            .filter(|(_, (end, _))| *end > range.start)
            .map_or(range.start, |(start, _)| *start)
    }

    /// Stored ranges overlapping `range`, in order.
    pub fn conflicts(&self, range: Range<u64>) -> impl Iterator<Item = (Range<u64>, &V)> + '_ {
        let first = self.first_start(&range);
        let end = range.end.max(first);
        self.inner
            .range(first..end)
            .map(|(start, (end, value))| (*start..*end, value))
    }

    pub fn iter(&self) -> impl Iterator<Item = (Range<u64>, &V)> + '_ {
        self.inner
            .iter()
            .map(|(start, (end, value))| (*start..*end, value))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Range<u64>, &mut V)> + '_ {
        self.inner
            .iter_mut()
            .map(|(start, (end, value))| (*start..*end, value))
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<V: Value> RangeMap<V> {
    /// Store `value` for `start..start + len`, merging with adjacent values.
    pub fn insert(&mut self, start: u64, len: u64, value: V) -> Result<(), Occupied> {
        let mut range = start..start + len;
        if range.is_empty() || self.conflicts(range.clone()).next().is_some() {
            return Err(Occupied);
        }

        let mut value = value;
        if let Some((&prev_start, (prev_end, prev))) = self.inner.range(..range.start).next_back() {
            if *prev_end == range.start {
                match value.try_merge_backwards(prev) {
                    Ok(merged) => {
                        value = merged;
                        range.start = prev_start;
                        self.inner.remove(&prev_start);
                    }
                    Err(unmerged) => value = unmerged,
                }
            }
        }
        if let Some((next_end, next)) = self.inner.get(&range.end) {
            let next_end = *next_end;
            match value.try_merge_forward(next) {
                Ok(merged) => {
                    value = merged;
                    self.inner.remove(&range.end);
                    range.end = next_end;
                }
                Err(unmerged) => value = unmerged,
            }
        }

        self.inner.insert(range.start, (range.end, value));
        Ok(())
    }

    /// Remove `range`, returning the removed parts in order. Stored ranges
    /// partly outside `range` keep their outside parts.
    pub fn remove(&mut self, range: Range<u64>) -> Vec<(Range<u64>, V)> {
        let starts: Vec<u64> = self
            .conflicts(range.clone())
            .map(|(r, _)| r.start)
            .collect();

        let mut removed = Vec::with_capacity(starts.len());
        for start in starts {
            let (end, value) = self.inner.remove(&start).unwrap();
            let middle = start.max(range.start)..end.min(range.end);
            let prev = Some(start..middle.start).filter(|r| !r.is_empty());
            let next = Some(middle.end..end).filter(|r| !r.is_empty());

            let (prev_value, value, next_value) =
                value.split(prev.clone(), middle.clone(), next.clone());
            if let (Some(prev), Some(prev_value)) = (prev, prev_value) {
                self.inner.insert(prev.start, (prev.end, prev_value));
            }
            if let (Some(next), Some(next_value)) = (next, next_value) {
                self.inner.insert(next.start, (next.end, next_value));
            }
            removed.push((middle, value));
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tags each unit of a range so splits can be checked.
    #[derive(Debug, PartialEq)]
    struct Tag(char);

    impl Value for Tag {
        fn try_merge_backwards(self, other: &Self) -> Result<Self, Self> {
            if self == *other {
                Ok(self)
            } else {
                Err(self)
            }
        }
        fn try_merge_forward(self, other: &Self) -> Result<Self, Self> {
            self.try_merge_backwards(other)
        }
        fn split(
            self,
            prev_range: Option<Range<u64>>,
            _range: Range<u64>,
            next_range: Option<Range<u64>>,
        ) -> (Option<Self>, Self, Option<Self>) {
            (
                prev_range.map(|_| Tag(self.0)),
                Tag(self.0),
                next_range.map(|_| Tag(self.0)),
            )
        }
    }

    fn contents(map: &RangeMap<Tag>) -> Vec<(Range<u64>, char)> {
        map.iter().map(|(r, t)| (r, t.0)).collect()
    }

    #[test]
    fn remove_wider_than_stored_range() {
        let mut map = RangeMap::new();
        map.insert(4, 4, Tag('a')).unwrap();

        let removed: Vec<_> = map
            .remove(0..12)
            .into_iter()
            .map(|(r, t)| (r, t.0))
            .collect();

        assert_eq!(removed, vec![(4..8, 'a')]);
        assert!(map.is_empty());
    }

    #[test]
    fn remove_adjacent_ranges() {
        let mut map = RangeMap::new();
        map.insert(0, 4, Tag('a')).unwrap();
        map.insert(4, 4, Tag('b')).unwrap();

        let removed: Vec<_> = map
            .remove(0..8)
            .into_iter()
            .map(|(r, t)| (r, t.0))
            .collect();

        assert_eq!(removed, vec![(0..4, 'a'), (4..8, 'b')]);
        assert!(map.is_empty());
    }

    #[test]
    fn remove_splits_stored_range() {
        let mut map = RangeMap::new();
        map.insert(0, 12, Tag('a')).unwrap();

        let removed: Vec<_> = map
            .remove(4..8)
            .into_iter()
            .map(|(r, t)| (r, t.0))
            .collect();

        assert_eq!(removed, vec![(4..8, 'a')]);
        assert_eq!(contents(&map), vec![(0..4, 'a'), (8..12, 'a')]);
    }

    #[test]
    fn insert_merges_equal_neighbours_only() {
        let mut map = RangeMap::new();
        map.insert(0, 4, Tag('a')).unwrap();
        map.insert(8, 4, Tag('a')).unwrap();
        map.insert(4, 4, Tag('a')).unwrap();
        map.insert(12, 4, Tag('b')).unwrap();

        assert_eq!(contents(&map), vec![(0..12, 'a'), (12..16, 'b')]);
    }

    #[test]
    fn insert_rejects_overlap() {
        let mut map = RangeMap::new();
        map.insert(4, 4, Tag('a')).unwrap();

        assert!(map.insert(0, 5, Tag('b')).is_err());
        assert!(map.insert(7, 4, Tag('b')).is_err());
        assert!(map.insert(0, 4, Tag('b')).is_ok());
    }

    #[test]
    fn conflicts_include_range_starting_before() {
        let mut map = RangeMap::new();
        map.insert(0, 8, Tag('a')).unwrap();
        map.insert(8, 4, Tag('b')).unwrap();
        map.insert(16, 4, Tag('c')).unwrap();

        let found: Vec<_> = map.conflicts(4..17).map(|(r, t)| (r, t.0)).collect();

        assert_eq!(found, vec![(0..8, 'a'), (8..12, 'b'), (16..20, 'c')]);
        assert_eq!(map.conflicts(12..16).count(), 0);
    }
}
