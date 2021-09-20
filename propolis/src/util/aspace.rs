//! Utilities to construct and manipulate an address space.

use std::collections::{btree_map, BTreeMap};
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::ops::RangeBounds;

/// Generic container storing items in a region representing an address space.
///
/// Stores ranges by (start, length), but also allows association
/// of generic objects with each region.
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
    ///
    /// # Panics
    ///
    /// - Panics if start >= end.
    pub fn new(start: usize, end: usize) -> ASpace<T> {
        assert!(start < end);
        Self { start, end, map: BTreeMap::new() }
    }

    /// Register an inclusive region [`start`, `end`].
    ///
    /// Returns an error if the region extends beyond the start/end of the
    /// address space, or if it conflicts with any existing registration.
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
            Some((_len, item)) => Ok(item),
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

    /// Get an iterator for all empty space, sorted by starting point
    ///
    /// Returns all space which does not overlap with registered regions.
    pub fn inverse_iter(&self) -> InverseIter<'_, T> {
        InverseIter { inner: self.map.iter(), next: 0, end: self.end }
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

    /// Clear all items from the space
    pub fn clear(&mut self) {
        self.map.clear();
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

/// Iterator for all items in an [ASpace], constructed by [ASpace::iter].
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

/// Represents a region in the space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extent {
    start: usize,
    len: usize,
}

impl Extent {
    pub fn start(&self) -> usize {
        self.start
    }
    pub fn len(&self) -> usize {
        self.len
    }
}

/// Iterator for empty space in an [ASpace], created by [ASpace::inverse_iter].
pub struct InverseIter<'a, T> {
    inner: btree_map::Iter<'a, usize, (usize, T)>,
    // Next potential empty region starting address.
    next: usize,
    end: usize,
}

impl<'a, T> Iterator for InverseIter<'a, T> {
    /// Item represents unregistered region in address mapping.
    type Item = Extent;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.end {
            match self.inner.next() {
                Some((registered_start, (registered_len, _))) => {
                    if self.next < *registered_start {
                        // Empty space exists before the next region.
                        let extent = Extent {
                            start: self.next,
                            len: *registered_start - self.next,
                        };
                        // Jump past the registered region.
                        self.next = *registered_start + registered_len;
                        return Some(extent);
                    } else {
                        // This space is registered. Move beyond it to find
                        // empty space.
                        self.next = *registered_start + registered_len;
                        continue;
                    }
                }
                None => {
                    // If we've run out of registered regions, return everything
                    // up to the end of the address space.
                    let extent =
                        Extent { start: self.next, len: self.end - self.next };
                    self.next = self.end;
                    return Some(extent);
                }
            }
        }
        None
    }
}

/// Iterator for items in an [ASpace] overlapping with a range, constructed by [ASpace::covered_by].
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

    #[test]
    fn inverse_iterator_alloc_middle() {
        let end = 100;
        let mut s: ASpace<()> = ASpace::new(0, end);

        // Registrations in the middle of the mapping, with free space at the
        // edges of the address space.
        assert!(s.register(10, 10, ()).is_ok());
        assert!(s.register(30, 10, ()).is_ok());

        let mut iter = s.inverse_iter();
        assert_eq!(Extent { start: 0, len: 10 }, iter.next().unwrap());
        assert_eq!(Extent { start: 20, len: 10 }, iter.next().unwrap());
        assert_eq!(Extent { start: 40, len: 60 }, iter.next().unwrap());
        assert!(iter.next().is_none());
    }

    #[test]
    fn inverse_iterator_alloc_ends() {
        let end = 100;
        let mut s: ASpace<()> = ASpace::new(0, end);

        // Registrations at the edges of the address space.
        assert!(s.register(0, 10, ()).is_ok());
        assert!(s.register(40, 20, ()).is_ok());
        assert!(s.register(90, 10, ()).is_ok());

        let mut iter = s.inverse_iter();
        assert_eq!(Extent { start: 10, len: 30 }, iter.next().unwrap());
        assert_eq!(Extent { start: 60, len: 30 }, iter.next().unwrap());
        assert!(iter.next().is_none());
    }

    #[test]
    fn inverse_iterator_sequential_registrations() {
        let end = 100;
        let mut s: ASpace<()> = ASpace::new(0, end);

        // Back-to-back registrations within the address space.
        assert!(s.register(10, 10, ()).is_ok());
        assert!(s.register(20, 10, ()).is_ok());
        assert!(s.register(30, 10, ()).is_ok());

        let mut iter = s.inverse_iter();
        assert_eq!(Extent { start: 0, len: 10 }, iter.next().unwrap());
        assert_eq!(Extent { start: 40, len: 60 }, iter.next().unwrap());
        assert!(iter.next().is_none());
    }

    #[test]
    fn inverse_iterator_empty() {
        let end = 100;
        let mut s: ASpace<()> = ASpace::new(0, end);

        // Entire address space occupied.
        assert!(s.register(0, 100, ()).is_ok());

        let mut iter = s.inverse_iter();
        assert!(iter.next().is_none());
    }

    #[test]
    fn inverse_iterator_full() {
        let end = 100;
        let s: ASpace<()> = ASpace::new(0, end);

        // Entire address space empty
        let mut iter = s.inverse_iter();
        assert_eq!(Extent { start: 0, len: 100 }, iter.next().unwrap());
        assert!(iter.next().is_none());
    }
}
