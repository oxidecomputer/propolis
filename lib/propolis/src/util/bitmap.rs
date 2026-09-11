// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

impl Default for Bitmap {
    fn default() -> Self {
        Bitmap(GenericBitmap([0u64; 1]))
    }
}

#[derive(Copy, Clone)]
pub(crate) struct Bitmap(GenericBitmap<1>);
impl Bitmap {
    const TOP_BIT: usize = GenericBitmap::<1>::TOP_BIT;

    pub const ALL: Self = Self(GenericBitmap::<1>::ALL);

    pub fn set(&mut self, idx: usize) {
        self.0.set(idx);
    }
    pub fn unset(&mut self, idx: usize) {
        self.0.unset(idx);
    }
    pub fn set_all(&mut self, other: Bitmap) {
        self.0.set_all(other.0);
    }
    pub fn lowest_set(&self) -> Option<usize> {
        self.0.lowest_set()
    }
    pub fn count(&self) -> usize {
        self.0.count()
    }
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
    pub fn take(&mut self) -> Self {
        Bitmap(self.0.take())
    }
    /// Get a copy of the underlying bits for this map.
    ///
    /// This is unlikely to be useful for any purpose other than debugging. For
    /// more "normal" use, consider [`Bitmap::take`].
    pub fn bits(&self) -> u64 {
        // This function expects the underlying bitmap is a u64 (which happens
        // to be expressed as `GenericBitmap<1>`). If that is no longer the
        // case, `bits` at the very least needs a new signature and the index
        // below is insufficient.
        assert_eq!(self.0.0.len(), 1);
        self.0.0[0]
    }
    /// Get iterator which emits indices of bits which are set in this map.
    pub fn iter(&self) -> BitIter {
        BitIter(self.0.iter())
    }
    /// Get iterator which emits indices of bits which are set in this map.
    /// It will infinitely loop back to the first bit whenever the last bit is
    /// reached.
    pub fn looping_iter(&self) -> LoopIter {
        LoopIter(self.0.looping_iter())
    }
}


/// Simple bitmap which facilitates iterator over bits which are asserted
// Some utility functions here may not be used at all times, but are here for
// convenience when they are needed.
#[allow(dead_code)]
#[derive(Copy, Clone)]
pub(crate) struct GenericBitmap<const T: usize>([u64; T]);
impl<const T: usize> GenericBitmap<T> {
    const TOP_BIT: usize = T * u64::BITS as usize;

    pub const ALL: Self = Self([u64::MAX; T]);

    pub fn set(&mut self, idx: usize) {
        assert!(idx < Self::TOP_BIT);
        self.0[idx / 64] |= 1u64 << idx;
    }
    pub fn unset(&mut self, idx: usize) {
        assert!(idx < Self::TOP_BIT);
        self.0[idx / 64] &= !(1u64 << idx);
    }
    pub fn set_all(&mut self, other: GenericBitmap<T>) {
        for i in 0..T {
            self.0[i] |= other.0[i];
        }
    }
    pub fn lowest_set(&self) -> Option<usize> {
        for i in 0..T {
            if self.0[i].count_ones() != 0 {
                return Some(self.0[i].trailing_zeros() as usize + i * 64);
            }
        }

        None
    }
    pub fn count(&self) -> usize {
        let mut sum = 0;
        for word in self.0.iter() {
            sum += word.count_ones() as usize;
        }
        sum
    }
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
    pub fn take(&mut self) -> Self {
        Self(std::mem::replace(&mut self.0, [0; T]))
    }
    /// Get a copy of the underlying bits for this map.
    ///
    /// This is unlikely to be useful for any purpose other than debugging. For
    /// more "normal" use, consider [`Bitmap::take`].
    pub fn bits(&self) -> [u64; T] {
        self.0
    }
    /// Get iterator which emits indices of bits which are set in this map.
    pub fn iter(&self) -> GenericBitIter<T> {
        GenericBitIter(*self)
    }
    /// Get iterator which emits indices of bits which are set in this map.
    /// It will infinitely loop back to the first bit whenever the last bit is
    /// reached.
    pub fn looping_iter(&self) -> GenericLoopIter<T> {
        GenericLoopIter { orig: *self, cur: *self }
    }
}

pub struct BitIter(GenericBitIter<1>);
impl Iterator for BitIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl BitIter {
    pub(crate) fn remainder(self) -> Bitmap {
        Bitmap(self.0.remainder())
    }
}

pub struct LoopIter(GenericLoopIter<1>);
impl Iterator for LoopIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

pub struct GenericBitIter<const T: usize>(GenericBitmap<T>);
impl<const T: usize> Iterator for GenericBitIter<T> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.0.lowest_set()?;
        self.0.unset(idx);
        Some(idx)
    }
}
impl<const T: usize> GenericBitIter<T> {
    pub(crate) fn remainder(self) -> GenericBitmap<T> {
        self.0
    }
}
pub struct GenericLoopIter<const T: usize> {
    cur: GenericBitmap<T>,
    orig: GenericBitmap<T>,
}
impl<const T: usize> Iterator for GenericLoopIter<T> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.orig.count() == 0 {
            return None;
        }
        if self.cur.count() == 0 {
            self.cur = self.orig;
        }
        let idx = self.cur.lowest_set().unwrap();
        self.cur.unset(idx);
        Some(idx)
    }
}
