use std::collections::{btree_map, BTreeMap};
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::ops::RangeBounds;

#[derive(Debug)]
pub struct ASpace<T> {
    start: usize,
    end: usize,
    map: BTreeMap<usize, (usize, T)>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    OutOfRange,
    BadLength,
    Conflict,
    NotFound,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Represents (start: usize, end: usize, item: &T) region in the space
type SpaceItem<'a, T> = (usize, usize, &'a T);

impl<T> ASpace<T> {
    /// Create a instance with inclusive range [`start`, `end`]
    pub fn new(start: usize, end: usize) -> ASpace<T> {
        assert!(start < end);
        Self { start, end, map: BTreeMap::new() }
    }

    /// Register an inclusive region [`start`, `end`]
    pub fn register(
        &mut self,
        start: usize,
        len: usize,
        item: T,
    ) -> Result<()> {
        let end = safe_end(start, len).ok_or(Error::BadLength)?;
        if start < self.start || start > self.end || end > self.end {
            return Err(Error::OutOfRange);
        }

        // Do any entries conflict with the registration?
        if self.covered_by((Included(start), Included(end))).next().is_some() {
            return Err(Error::Conflict);
        }

        let was_overlap = self.map.insert(start, (len, item));
        assert!(was_overlap.is_none());
        Ok(())
    }

    /// Unregister region which begins at `start`
    pub fn unregister(&mut self, start: usize) -> Result<T> {
        match self.map.remove(&start) {
            Some((len, item)) => Ok(item),
            None => Err(Error::NotFound),
        }
    }

    /// Search for region which contains `point`
    pub fn region_at(&self, point: usize) -> Result<SpaceItem<'_, T>> {
        if point < self.start || point > self.end {
            return Err(Error::OutOfRange);
        }
        if let Some((start, ent)) =
            self.map.range((Unbounded, Included(&point))).next_back()
        {
            if safe_end(*start, ent.0).unwrap() >= point {
                return Ok((*start, ent.0, &ent.1));
            }
        }
        Err(Error::NotFound)
    }

    /// Get an iterator for items in the space, sorted by starting point
    pub fn iter(&self) -> Iter<'_, T> {
        Iter { inner: self.map.iter() }
    }

    /// Get iterator for regions which are (partially or totally) covered by a range
    pub fn covered_by<R>(&self, range: R) -> Range<'_, T>
    where
        R: RangeBounds<usize>,
    {
        // The front bound needs to be adjusted to search for any region which preceeds the start
        // point, since that region may extend into the target search range.
        let fixed_front = match range.start_bound() {
            Unbounded => Unbounded,
            Excluded(pos) => {
                if let Ok((start, _, _)) = self.region_at(pos + 1) {
                    Included(start)
                } else {
                    Excluded(*pos)
                }
            }
            Included(pos) => {
                if let Ok((start, _, _)) = self.region_at(*pos) {
                    Included(start)
                } else {
                    Excluded(*pos)
                }
            }
        };
        let tail = match range.end_bound() {
            Unbounded => Unbounded,
            Excluded(a) => Excluded(*a),
            Included(a) => Included(*a),
        };
        Range { inner: self.map.range((fixed_front, tail)) }
    }
}

fn safe_end(start: usize, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else if start == 0 {
        Some((start + len) - 1)
    } else {
        (start - 1).checked_add(len)
    }
}

// Flatten the K/V nested tuple
fn kv_flatten<'a, T>(i: (&'a usize, &'a (usize, T))) -> SpaceItem<'a, T> {
    let start = *i.0;
    let end = (i.1).0;
    let item = &(i.1).1;
    (start, end, item)
}

pub struct Iter<'a, T> {
    inner: btree_map::Iter<'a, usize, (usize, T)>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    /// Item represents (start, end, &item)
    type Item = SpaceItem<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(kv_flatten)
    }
}

pub struct Range<'a, T> {
    inner: btree_map::Range<'a, usize, (usize, T)>,
}

impl<'a, T> Iterator for Range<'a, T> {
    /// Item represents (start, end, &item)
    type Item = SpaceItem<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(kv_flatten)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic]
    fn create_zero_size() {
        let _s: ASpace<u32> = ASpace::new(0, 0);
    }
    #[test]
    fn create_max() {
        let _s: ASpace<u32> = ASpace::new(0, usize::max_value());
    }
    #[test]
    fn create_normal() {
        let _s: ASpace<u32> = ASpace::new(0x1000, 0xffff);
    }
    #[test]
    fn register_plain() {
        let mut s: ASpace<u32> = ASpace::new(0, 0xffff);

        assert!(s.register(0, 0x1000, 0).is_ok());
    }
    #[test]
    fn register_invalid() {
        let mut s: ASpace<u32> = ASpace::new(0, 0x1000);

        assert_eq!(s.register(0x100, 0, 0), Err(Error::BadLength));
        assert_eq!(
            s.register(0x100, usize::MAX - 0x50, 0),
            Err(Error::BadLength)
        );
    }
    #[test]
    fn register_outside() {
        let start = 0x100;
        let end = 0x1ff;
        let len = end - start + 1;
        let mut s: ASpace<u32> = ASpace::new(start, end);

        let expect: Result<()> = Err(Error::OutOfRange);
        assert_eq!(s.register(0, start - 1, 0), expect);
        assert_eq!(s.register(0, start, 0), expect);
        assert_eq!(s.register(0, start + 1, 0), expect);

        assert_eq!(s.register(start + 1, len, 0), expect);
        assert_eq!(s.register(start + len, 1, 0), expect);
    }
    #[test]
    fn register_overlaps() {
        let mut s: ASpace<u32> = ASpace::new(0, 0xffff);
        assert!(s.register(0, 0x1000, 0).is_ok());
        assert!(s.register(0x2000, 0x1000, 0).is_ok());
        assert!(s.register(0xf000, 0x1000, 0).is_ok());
        let expect: Result<()> = Err(Error::Conflict);

        // direct overlap
        assert_eq!(s.register(0, 0x1000, 0), expect);
        assert_eq!(s.register(0x2000, 0x1000, 0), expect);
        assert_eq!(s.register(0xf000, 0x1000, 0), expect);

        // tail overlap
        assert_eq!(s.register(0x1ff0, 0x0011, 0), expect);
        assert_eq!(s.register(0x1ff0, 0x0012, 0), expect);
        assert_eq!(s.register(0x1ff0, 0x1000, 0), expect);
        assert_eq!(s.register(0xefff, 0x0002, 0), expect);
        assert_eq!(s.register(0xefff, 0x0003, 0), expect);
        assert_eq!(s.register(0xefff, 0x1000, 0), expect);

        // head overlap
        assert_eq!(s.register(0x0ffe, 0x10, 0), expect);
        assert_eq!(s.register(0x0fff, 0x10, 0), expect);
        assert_eq!(s.register(0x2ffe, 0x10, 0), expect);
        assert_eq!(s.register(0x2fff, 0x10, 0), expect);

        // total overlap
        assert_eq!(s.register(0x1ff0, 0x1010, 0), expect);
    }
    #[test]
    fn region_at_outside() {
        let end = 0xffff;
        let mut s: ASpace<u32> = ASpace::new(0, end);

        assert!(s.register(0x1000, 0x1000, 0).is_ok());
        assert_eq!(s.region_at(end + 1), Err(Error::OutOfRange));
        assert_eq!(s.region_at(end + 10), Err(Error::OutOfRange));
    }
    #[test]
    fn region_at_normal() {
        let end = 0xffff;
        let mut s: ASpace<u32> = ASpace::new(0, end);

        let ent: [(usize, usize, &u32); 3] = [
            (0x100, 0x100, &0),
            (0x2000, 0x1000, &1),
            (end - 0xfff, 0x1000, &2),
        ];
        for (a, b, c) in ent.iter() {
            assert!(s.register(*a, *b, **c).is_ok());
        }

        assert_eq!(s.region_at(0x100), Ok(ent[0]));
        assert_eq!(s.region_at(0x110), Ok(ent[0]));
        assert_eq!(s.region_at(0x1ff), Ok(ent[0]));
        assert_eq!(s.region_at(0x2990), Ok(ent[1]));
        assert_eq!(s.region_at(0x2fff), Ok(ent[1]));
        assert_eq!(s.region_at(0xfff0), Ok(ent[2]));
        assert_eq!(s.region_at(end), Ok(ent[2]));

        assert_eq!(s.region_at(0), Err(Error::NotFound));
        assert_eq!(s.region_at(0x200), Err(Error::NotFound));
        assert_eq!(s.region_at(0x5000), Err(Error::NotFound));

        assert_eq!(s.region_at(end + 1), Err(Error::OutOfRange));
        assert_eq!(s.region_at(end + 10), Err(Error::OutOfRange));
    }
}
