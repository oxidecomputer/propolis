use std::collections::{btree_map, BTreeMap};
use std::ops::Bound::{Included, Unbounded};

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

impl<T> ASpace<T> {
    /// Create a instance with inclusive range [`start`, `end`]
    pub fn new(start: usize, end: usize) -> ASpace<T> {
        assert!(start < end);
        Self {
            start,
            end,
            map: BTreeMap::new(),
        }
    }

    /// Register an inclusive region [`start`, `end`]
    pub fn register(&mut self, start: usize, end: usize, item: T) -> Result<()> {
        if start < self.start || start > self.end || end > self.end {
            return Err(Error::OutOfRange);
        }
        if end < start {
            return Err(Error::BadLength);
        }

        // Do any entries start within the space?
        if self
            .map
            .range((Included(&start), Included(&end)))
            .next()
            .is_some()
        {
            return Err(Error::Conflict);
        }

        // Does a preceeding entry spill into the space?
        let preceeding = self.map.range((Unbounded, Included(&start))).next_back();
        if let Some((_p_start, ent)) = preceeding {
            if ent.0 >= start {
                return Err(Error::Conflict);
            }
        }

        let was_overlap = self.map.insert(start, (end, item));
        assert!(was_overlap.is_none());
        Ok(())
    }

    /// Unregister region which begins at `start`
    pub fn unregister(&mut self, start: usize) -> Result<()> {
        match self.map.remove(&start) {
            Some(_) => Ok(()),
            None => Err(Error::NotFound),
        }
    }

    /// Search for region which contains `point`
    pub fn region_at(&self, point: usize) -> Result<(usize, usize, &T)> {
        if point < self.start || point > self.end {
            return Err(Error::OutOfRange);
        }
        match self.map.range((Unbounded, Included(&point))).next_back() {
            Some((start, ent)) if ent.0 > point => Ok((*start, ent.0, &ent.1)),
            _ => Err(Error::NotFound),
        }
    }

    /// Get an iterator for items in the space, sorted by starting point
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            inner: self.map.iter(),
        }
    }
}

pub struct Iter<'a, T> {
    inner: btree_map::Iter<'a, usize, (usize, T)>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    /// Item represents (start, end, &item)
    type Item = (usize, usize, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, v)| (*k, v.0, &v.1))
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

        assert_eq!(s.register(0x100, 0xff, 0), Err(Error::BadLength));
    }
    #[test]
    fn register_outside() {
        let start = 0x100;
        let end = 0x1ff;
        let mut s: ASpace<u32> = ASpace::new(start, end);

        let expect: Result<()> = Err(Error::OutOfRange);
        assert_eq!(s.register(0, start - 1, 0), expect);
        assert_eq!(s.register(0, start, 0), expect);
        assert_eq!(s.register(0, start + 1, 0), expect);

        assert_eq!(s.register(start + 1, end + 1, 0), expect);
        assert_eq!(s.register(end, end + 1, 0), expect);
        assert_eq!(s.register(end, end + 2, 0), expect);
    }
    #[test]
    fn register_overlaps() {
        let mut s: ASpace<u32> = ASpace::new(0, 0xffff);
        assert!(s.register(0, 0xfff, 0).is_ok());
        assert!(s.register(0x2000, 0x2fff, 0).is_ok());
        assert!(s.register(0xf000, 0xffff, 0).is_ok());
        let expect: Result<()> = Err(Error::Conflict);

        // direct overlap
        assert_eq!(s.register(0, 0xfff, 0), expect);
        assert_eq!(s.register(0x2000, 0x2fff, 0), expect);
        assert_eq!(s.register(0xf000, 0xffff, 0), expect);

        // tail overlap
        assert_eq!(s.register(0x1ff0, 0x2000, 0), expect);
        assert_eq!(s.register(0x1ff0, 0x2001, 0), expect);
        assert_eq!(s.register(0x1ff0, 0x3000, 0), expect);
        assert_eq!(s.register(0xefff, 0xf000, 0), expect);
        assert_eq!(s.register(0xefff, 0xf001, 0), expect);
        assert_eq!(s.register(0xefff, 0xffff, 0), expect);

        // head overlap
        assert_eq!(s.register(0x0ffe, 0x1010, 0), expect);
        assert_eq!(s.register(0x0fff, 0x1010, 0), expect);
        assert_eq!(s.register(0x2ffe, 0x3010, 0), expect);
        assert_eq!(s.register(0x2fff, 0x3010, 0), expect);

        // total overlap
        assert_eq!(s.register(0x1ff0, 0x3010, 0), expect);
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

        let ent: [(usize, usize, &u32); 6] = [
            (0x100, 0x1ff, &0),
            (0x1000, 0x10ff, &1),
            (0x1100, 0x11ff, &2),
            (0x1200, 0x12ff, &3),
            (0x2000, 0x2fff, &4),
            (end - 0xfff, end, &5),
        ];
        for (a, b, c) in ent.iter() {
            assert!(s.register(*a, *b, **c).is_ok());
        }

        assert_eq!(s.region_at(0x100), Ok(ent[0]));
        assert_eq!(s.region_at(0x110), Ok(ent[0]));
        assert_eq!(s.region_at(0x1050), Ok(ent[1]));
        assert_eq!(s.region_at(0x1100), Ok(ent[2]));
        assert_eq!(s.region_at(0x1150), Ok(ent[2]));
        assert_eq!(s.region_at(0x1250), Ok(ent[3]));
        assert_eq!(s.region_at(0x2990), Ok(ent[4]));
        assert_eq!(s.region_at(0xfff0), Ok(ent[5]));

        assert_eq!(s.region_at(0), Err(Error::NotFound));
        assert_eq!(s.region_at(0x200), Err(Error::NotFound));
        assert_eq!(s.region_at(0x5000), Err(Error::NotFound));

        assert_eq!(s.region_at(end + 1), Err(Error::OutOfRange));
        assert_eq!(s.region_at(end + 10), Err(Error::OutOfRange));
    }
}
