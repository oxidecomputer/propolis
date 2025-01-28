// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility functions and types for working with CPUID values.
//!
//! # The `CPUID` instruction
//!
//! The x86 CPUID instruction returns information about the executing processor.
//! This information can range from a manufacturer ID to a model number to the
//! processor's supported feature set to the calling logical processor's APIC
//! ID.
//!
//! CPUID takes as input a "leaf" or "function" value, passed in the eax
//! register, which determines what information the processor should return.
//! Some leaves accept a "subleaf" or "index" value, specified in ecx, that
//! further qualifies the information class supplied in eax. For example, leaf 4
//! returns information about a processor's cache topology; it uses the subleaf
//! value as an index that identifies a particular type and level of cache.
//!
//! The CPUID leaf space is divided into "standard" and "extended" regions.
//! Leaves in the standard region (0 to 0xFFFF) have architecturally-defined
//! semantics. Leaves in the extended region (0x80000000 to 0x8000FFFF) have
//! vendor-specific semantics.
//!
//! `propolis-server` can accept a list of CPUID leaf/subleaf/value tuples as
//! part of an instance specification. If a client supplies one, Propolis will
//! initialize each vCPU so that CPUID instructions executed there will return
//! the supplied values, possibly with some adjustments (substituting vCPU
//! numbers into those leaves that return them, adjusting CPU topology leaves
//! based on other settings in the spec, etc.).
//!
//! This module implements two collections that Propolis components can use to
//! hold CPUID information:
//!
//! - [`CpuidMap`] maps from CPUID leaf and subleaf pairs to their associated
//!   values, taking care to ensure that a single leaf is marked either as
//!   ignoring or honoring the subleaf number, but not both.
//! - [`CpuidSet`] pairs a `CpuidMap` with a [`CpuidVendor`] that Propolis can
//!   use to interpret the values of the map leaves in the extended region.
//!
//! # The `instance-spec` feature
//!
//! If this crate is built with the `instance-spec` feature, this module
//! includes mechanisms for converting from instance spec CPUID entries to and
//! from the CPUID map types in the crate. This is feature-gated so that the
//! main Propolis lib can use this library without depending on
//! `propolis-api-types`.

use std::{
    collections::{
        btree_map::{self, Entry},
        BTreeMap, BTreeSet,
    },
    ops::RangeInclusive,
};

pub use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor};
use thiserror::Error;

pub mod bits;
pub mod host;

#[cfg(feature = "instance-spec")]
mod instance_spec;

type CpuidSubleafMap = BTreeMap<u32, CpuidValues>;
type CpuidMapInsertResult = Result<Option<CpuidValues>, CpuidMapInsertError>;

/// Denotes the presence or absence of subleaves for a given CPUID leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Subleaves {
    /// This leaf is configured to have subleaves, whose values are stored in
    /// the inner map.
    Present(CpuidSubleafMap),

    /// This leaf does not have subleaves.
    Absent(CpuidValues),
}

/// [`CpuidMap`]'s insert functions return this error if a request to insert a
/// new value would produce a leaf that has both per-subleaf values and a
/// no-subleaf value.
#[derive(Debug, Error)]
pub enum CpuidMapInsertError {
    #[error("leaf {0:x} has entries with subleaves")]
    SubleavesAlreadyPresent(u32),

    #[error("leaf {0:x} has an entry with no subleaves")]
    SubleavesAlreadyAbsent(u32),
}

/// A mapping from CPUID leaf/subleaf pairs to CPUID return values. This struct
/// allows each registered leaf either to have or not to have subleaf values,
/// but not both at once.
///
/// Some CPUID leaves completely ignore the subleaf value passed in ecx. Others
/// pay attention to it and have their own per-leaf semantics that govern what
/// happens if the supplied ecx value is out of the function's expected range.
/// Tracking CPUID values in a simple `BTreeMap` from [`CpuidIdent`] to
/// [`CpuidValues`] allows a single leaf to use both options at once, because
/// `CpuidIdent { leaf: x, subleaf: None }` is not the same as `CpuidIdent {
/// leaf: x, subleaf: Some(y) }`. To avoid this kind of semantic confusion, this
/// type's `insert` method returns a `Result` that indicates whether inserting
/// the requested leaf/subleaf identifier would produce a semantic conflict
/// between a subleaf-bearing and subleaf-free entry.
///
/// This structure allows "holes" in a leaf's subleaf IDs; that is, the
/// structure permits `leaf: 0, subleaf: Some(0)` and `subleaf: Some(2)` to
/// appear in a map where `subleaf: Some(1)` is absent. This is mostly for
/// simplicity (it saves the map type from having to check for discontiguous
/// subleaf domains); the existing subleaf-having CPUID leaves in the Intel and
/// AMD manuals all specify contiguous subleaf domains, and a client who
/// specifies a discontiguous subleaf set may find itself with unhappy guest
/// operating systems.
#[derive(Clone, Debug, Default)]
pub struct CpuidMap(BTreeMap<u32, Subleaves>);

impl CpuidMap {
    /// Retrieves the values associated with the supplied `ident`, or `None` if
    /// the identifier is not present in the map.
    pub fn get(&self, ident: CpuidIdent) -> Option<&CpuidValues> {
        match ident.subleaf {
            None => self.0.get(&ident.leaf).map(|ent| match ent {
                Subleaves::Present(_) => None,
                Subleaves::Absent(val) => Some(val),
            }),
            Some(sl) => self.0.get(&ident.leaf).map(|ent| match ent {
                Subleaves::Absent(_) => None,
                Subleaves::Present(sl_map) => sl_map.get(&sl),
            }),
        }
        .flatten()
    }

    /// Retrieves a mutable reference to the values associated with the supplied
    /// `ident`, or `None` if the identifier is not present in the map.
    pub fn get_mut(&mut self, ident: CpuidIdent) -> Option<&mut CpuidValues> {
        match ident.subleaf {
            None => self.0.get_mut(&ident.leaf).map(|ent| match ent {
                Subleaves::Present(_) => None,
                Subleaves::Absent(val) => Some(val),
            }),
            Some(sl) => self.0.get_mut(&ident.leaf).map(|ent| match ent {
                Subleaves::Absent(_) => None,
                Subleaves::Present(sl_map) => sl_map.get_mut(&sl),
            }),
        }
        .flatten()
    }

    fn insert_leaf_no_subleaf(
        &mut self,
        leaf: u32,
        values: CpuidValues,
    ) -> CpuidMapInsertResult {
        match self.0.entry(leaf) {
            Entry::Vacant(e) => {
                e.insert(Subleaves::Absent(values));
                Ok(None)
            }
            Entry::Occupied(mut e) => match e.get_mut() {
                Subleaves::Present(_) => {
                    Err(CpuidMapInsertError::SubleavesAlreadyPresent(leaf))
                }
                Subleaves::Absent(v) => Ok(Some(std::mem::replace(v, values))),
            },
        }
    }

    fn insert_leaf_subleaf(
        &mut self,
        leaf: u32,
        subleaf: u32,
        values: CpuidValues,
    ) -> CpuidMapInsertResult {
        match self.0.entry(leaf) {
            Entry::Vacant(e) => {
                e.insert(Subleaves::Present(
                    [(subleaf, values)].into_iter().collect(),
                ));
                Ok(None)
            }
            Entry::Occupied(mut e) => match e.get_mut() {
                Subleaves::Absent(_) => {
                    Err(CpuidMapInsertError::SubleavesAlreadyAbsent(leaf))
                }
                Subleaves::Present(sl_map) => {
                    Ok(sl_map.insert(subleaf, values))
                }
            },
        }
    }

    /// Inserts the supplied (`ident`, `values`) pair into the map.
    ///
    /// # Return value
    ///
    /// - `Ok(None)` if the supplied leaf/subleaf pair was not present in the
    ///   map.
    /// - `Ok(Some)` if the supplied leaf/subleaf pair was present in the map.
    ///   The wrapped value is the previous value set for this pair, which is
    ///   replaced by the supplied `values`.
    /// - `Err` if the insert would cause the selected leaf to have one entry
    ///   with no subleaf and one entry with a subleaf.
    pub fn insert(
        &mut self,
        ident: CpuidIdent,
        values: CpuidValues,
    ) -> CpuidMapInsertResult {
        match ident.subleaf {
            Some(sl) => self.insert_leaf_subleaf(ident.leaf, sl, values),
            None => self.insert_leaf_no_subleaf(ident.leaf, values),
        }
    }

    /// Removes the entry with the supplied `ident` from the map if it is
    /// present, returning its value.
    ///
    /// If `ident.subleaf` is `None`, this routine will only match leaf entries
    /// that don't specify per-subleaf values.
    pub fn remove(&mut self, ident: CpuidIdent) -> Option<CpuidValues> {
        // If the leaf isn't present there's nothing to return.
        let Entry::Occupied(mut entry) = self.0.entry(ident.leaf) else {
            return None;
        };

        let (val, remove_leaf) = {
            match (ident.subleaf, entry.get_mut()) {
                // The caller didn't supply a subleaf, and this leaf doesn't
                // have a subleaf map. Yank the leaf entry and return the
                // associated value. (This can't be done inline because the
                // entry is mutably borrowed.)
                (None, Subleaves::Absent(val)) => (*val, true),
                // The caller didn't supply a subleaf, but the leaf has one, so
                // the keys don't match.
                (None, Subleaves::Present(_)) => {
                    return None;
                }
                // The caller supplied a subleaf, but the leaf doesn't use
                // subleaves, so the keys don't match.
                (Some(_), Subleaves::Absent(_)) => {
                    return None;
                }
                // The caller supplied a subleaf, and this leaf has subleaf
                // data. If the requested subleaf is in the leaf's subleaf map,
                // remove the corresponding subleaf entry. If this empties the
                // subleaf map, also clean up the leaf entry.
                (Some(subleaf), Subleaves::Present(sl_map)) => {
                    let val = sl_map.remove(&subleaf)?;
                    (val, sl_map.is_empty())
                }
            }
        };

        if remove_leaf {
            entry.remove_entry();
        }

        Some(val)
    }

    /// Removes all data for the supplied `leaf`. If the leaf has subleaves, all
    /// their entries are removed.
    pub fn remove_leaf(&mut self, leaf: u32) {
        self.0.remove(&leaf);
    }

    /// Retains only the entries in this map for which `f` returns `true`.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(CpuidIdent, CpuidValues) -> bool,
    {
        self.0.retain(|leaf, subleaves| match subleaves {
            Subleaves::Absent(v) => f(CpuidIdent::leaf(*leaf), *v),
            Subleaves::Present(sl_map) => {
                sl_map.retain(|subleaf, v| {
                    f(CpuidIdent::subleaf(*leaf, *subleaf), *v)
                });
                !sl_map.is_empty()
            }
        })
    }

    /// Clears the entire map.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Returns `true` if the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the total number of leaf/subleaf/value tuples in the map.
    pub fn len(&self) -> usize {
        let mut len = 0;
        for sl in self.0.values() {
            match sl {
                Subleaves::Absent(_) => len += 1,
                Subleaves::Present(sl_map) => len += sl_map.len(),
            }
        }

        len
    }

    /// Returns an iterator over the ([`CpuidIdent`], [`CpuidValues`]) pairs in
    /// the map.
    pub fn iter(&self) -> CpuidMapIterator {
        CpuidMapIterator::new(self)
    }
}

/// An iterator over a [`CpuidMap`]'s leaf/subleaf/value tuples.
pub struct CpuidMapIterator<'a> {
    leaf_iter: btree_map::Iter<'a, u32, Subleaves>,
    subleaf_iter: Option<(u32, btree_map::Iter<'a, u32, CpuidValues>)>,
}

impl<'a> CpuidMapIterator<'a> {
    fn new(map: &'a CpuidMap) -> Self {
        Self { leaf_iter: map.0.iter(), subleaf_iter: None }
    }
}

impl Iterator for CpuidMapIterator<'_> {
    type Item = (CpuidIdent, CpuidValues);

    fn next(&mut self) -> Option<Self::Item> {
        // If a subleaf iteration is in progress, try to advance that iterator.
        // If that produces another subleaf value, return it. Otherwise, clear
        // the subleaf iterator and move to the next leaf.
        if let Some((leaf, subleaf_iter)) = &mut self.subleaf_iter {
            if let Some((subleaf, value)) = subleaf_iter.next() {
                return Some((CpuidIdent::subleaf(*leaf, *subleaf), *value));
            };

            self.subleaf_iter = None;
        }

        // Advance the leaf iterator. If there are no more leaves to iterate,
        // the entire iteration is over.
        let (leaf, subleaves) = self.leaf_iter.next()?;

        // Iteration has moved to a new leaf. Consider whether it has any
        // subleaves. If it doesn't, simply return the leaf and value.
        // Otherwise, start iterating over subleaves.
        match subleaves {
            Subleaves::Absent(val) => Some((CpuidIdent::leaf(*leaf), *val)),
            Subleaves::Present(sl_map) => {
                let mut subleaf_iter = sl_map.iter();

                // This invariant is upheld by insert/remove.
                let (subleaf, value) = subleaf_iter
                    .next()
                    .expect("subleaf maps always have at least one entry");

                // Stash the iterator along with the leaf value it
                // corresponds to so that future iterations can return
                // the entire leaf/subleaf pair.
                self.subleaf_iter = Some((*leaf, subleaf_iter));
                Some((CpuidIdent::subleaf(*leaf, *subleaf), *value))
            }
        }
    }
}

/// A map from CPUID leaves to CPUID values that includes a vendor ID. Callers
/// can use the vendor ID to interpret the meanings of any extended leaves
/// present in the map.
#[derive(Clone, Debug)]
pub struct CpuidSet {
    map: CpuidMap,
    vendor: CpuidVendor,
}

impl Default for CpuidSet {
    /// Equivalent to [`Self::new_host`].
    fn default() -> Self {
        Self::new_host()
    }
}

/// A discrepancy between two [`CpuidSet`]s.
#[derive(Debug, Error)]
pub enum CpuidSetMismatch {
    /// The sets have different CPU vendors.
    #[error("CPUID set has mismatched vendors (self: {this}, other: {other})")]
    Vendor { this: CpuidVendor, other: CpuidVendor },

    /// The two sets have different leaves or subleaves. The payload contains
    /// all of the leaf identifiers that were present in one set but not the
    /// other.
    #[error("CPUID leaves not found in both sets ({0:?})")]
    LeafSet(Vec<CpuidIdent>),

    /// The two sets disagree on the values to return for one or more
    /// leaf/subleaf pairs. The payload contains the leaf/subleaf ID and values
    /// for all such pairs.
    #[error("CPUID leaves have different values in different sets ({0:?})")]
    Values(Vec<(CpuidIdent, CpuidValues, CpuidValues)>),
}

impl CpuidSet {
    /// Creates an empty `CpuidSet` with the supplied `vendor`.
    pub fn new(vendor: CpuidVendor) -> Self {
        Self { map: CpuidMap::default(), vendor }
    }

    /// Creates a new `CpuidSet` with the supplied initial leaf/value `map` and
    /// `vendor`.
    pub fn from_map(map: CpuidMap, vendor: CpuidVendor) -> Self {
        Self { map, vendor }
    }

    /// Yields this set's vendor.
    pub fn vendor(&self) -> CpuidVendor {
        self.vendor
    }

    /// Executes the CPUID instruction on the current machine to determine its
    /// processor vendor, then creates an empty `CpuidSet` with that vendor.
    ///
    /// # Panics
    ///
    /// Panics if the host is not an Intel or AMD CPU (leaf 0 ebx/ecx/edx
    /// contain something other than "GenuineIntel" or "AuthenticAMD").
    pub fn new_host() -> Self {
        let vendor = CpuidVendor::try_from(host::query(CpuidIdent::leaf(0)))
            .expect("host CPU should be from recognized vendor");
        Self::new(vendor)
    }

    /// See [`CpuidMap::insert`].
    pub fn insert(
        &mut self,
        ident: CpuidIdent,
        values: CpuidValues,
    ) -> CpuidMapInsertResult {
        self.map.insert(ident, values)
    }

    /// See [`CpuidMap::get`].
    pub fn get(&self, ident: CpuidIdent) -> Option<&CpuidValues> {
        self.map.get(ident)
    }

    /// See [`CpuidMap::get_mut`].
    pub fn get_mut(&mut self, ident: CpuidIdent) -> Option<&mut CpuidValues> {
        self.map.get_mut(ident)
    }

    /// See [`CpuidMap::remove_leaf`].
    pub fn remove_leaf(&mut self, leaf: u32) {
        self.map.remove_leaf(leaf);
    }

    /// See [`CpuidMap::retain`].
    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(CpuidIdent, CpuidValues) -> bool,
    {
        self.map.retain(f);
    }

    /// See [`CpuidMap::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// See [`CpuidMap::iter`].
    pub fn iter(&self) -> CpuidMapIterator {
        self.map.iter()
    }

    /// Returns `Ok` if `self` is equivalent to `other`; if not, returns a
    /// [`CpuidMapMismatch`] describing the first observed difference between
    /// the two sets.
    pub fn is_equivalent_to(
        &self,
        other: &Self,
    ) -> Result<(), CpuidSetMismatch> {
        if self.vendor != other.vendor {
            return Err(CpuidSetMismatch::Vendor {
                this: self.vendor,
                other: other.vendor,
            });
        }

        let this_set: BTreeSet<_> =
            self.map.iter().map(|(ident, _)| ident).collect();
        let other_set: BTreeSet<_> =
            other.map.iter().map(|(ident, _)| ident).collect();
        let diff = this_set.symmetric_difference(&other_set);
        let diff: Vec<CpuidIdent> = diff.copied().collect();

        if !diff.is_empty() {
            return Err(CpuidSetMismatch::LeafSet(diff));
        }

        let mut mismatches = vec![];
        for (this_leaf, this_value) in self.map.iter() {
            let other_value = other
                .map
                .get(this_leaf)
                .expect("key sets were already found to be equal");

            if this_value != *other_value {
                mismatches.push((this_leaf, this_value, *other_value));
            }
        }

        if !mismatches.is_empty() {
            Err(CpuidSetMismatch::Values(mismatches))
        } else {
            Ok(())
        }
    }
}

impl From<CpuidSet> for Vec<bhyve_api::vcpu_cpuid_entry> {
    fn from(value: CpuidSet) -> Self {
        let mut out = Vec::with_capacity(value.map.len());
        out.extend(value.map.iter().map(|(ident, leaf)| {
            let vce_flags = match ident.subleaf.as_ref() {
                Some(_) => bhyve_api::VCE_FLAG_MATCH_INDEX,
                None => 0,
            };
            bhyve_api::vcpu_cpuid_entry {
                vce_function: ident.leaf,
                vce_index: ident.subleaf.unwrap_or(0),
                vce_flags,
                vce_eax: leaf.eax,
                vce_ebx: leaf.ebx,
                vce_ecx: leaf.ecx,
                vce_edx: leaf.edx,
                ..Default::default()
            }
        }));
        out
    }
}

/// An error that can occur when converting a list of CPUID entries in an
/// instance spec into a [`CpuidMap`].
#[derive(Debug, Error)]
pub enum CpuidMapConversionError {
    #[error("duplicate leaf and subleaf ({0:x}, {1:?})")]
    DuplicateLeaf(u32, Option<u32>),

    #[error("leaf {0:x} not in standard or extended range")]
    LeafOutOfRange(u32),

    #[error(transparent)]
    SubleafConflict(#[from] CpuidMapInsertError),
}

/// The range of standard, architecturally-defined CPUID leaves.
pub const STANDARD_LEAVES: RangeInclusive<u32> = 0..=0xFFFF;

/// The range of extended CPUID leaves. The meanings of these leaves are CPU
/// vendor-specific.
pub const EXTENDED_LEAVES: RangeInclusive<u32> = 0x8000_0000..=0x8000_FFFF;

#[cfg(test)]
mod test {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn insert_leaf_then_subleaf_fails() {
        let mut map = CpuidMap::default();
        assert_eq!(
            map.insert(CpuidIdent::leaf(0), CpuidValues::default()).unwrap(),
            None
        );

        map.insert(CpuidIdent::subleaf(0, 0), CpuidValues::default())
            .unwrap_err();
    }

    #[test]
    fn insert_subleaf_then_leaf_fails() {
        let mut map = CpuidMap::default();
        assert_eq!(
            map.insert(CpuidIdent::subleaf(0, 0), CpuidValues::default())
                .unwrap(),
            None
        );

        map.insert(CpuidIdent::leaf(0), CpuidValues::default()).unwrap_err();
    }

    #[test]
    fn insert_leaf_then_remove_then_insert_subleaf() {
        let mut map = CpuidMap::default();
        let values = CpuidValues { eax: 1, ebx: 2, ecx: 3, edx: 4 };
        assert_eq!(map.insert(CpuidIdent::leaf(0), values).unwrap(), None);

        map.insert(CpuidIdent::subleaf(0, 1), values).unwrap_err();
        assert_eq!(map.remove(CpuidIdent::leaf(0)).unwrap(), values);
        assert_eq!(
            map.insert(CpuidIdent::subleaf(0, 1), CpuidValues::default())
                .unwrap(),
            None
        );
    }

    #[test]
    fn insert_multiple_subleaves() {
        let mut map = CpuidMap::default();
        let values = CpuidValues { eax: 1, ebx: 2, ecx: 3, edx: 4 };
        for subleaf in 0..10 {
            assert_eq!(
                map.insert(CpuidIdent::subleaf(0, subleaf), values).unwrap(),
                None
            );
        }
    }

    #[test]
    fn leaf_subleaf_removal() {
        let mut map = CpuidMap::default();

        // Add some leaves (0-2) with no subleaves.
        for leaf in 0..3 {
            assert_eq!(
                map.insert(CpuidIdent::leaf(leaf), CpuidValues::default())
                    .unwrap(),
                None
            );
        }

        // Add some leaves (3-5) with subleaves.
        for leaf in 3..6 {
            for subleaf in 6..9 {
                assert_eq!(
                    map.insert(
                        CpuidIdent::subleaf(leaf, subleaf),
                        CpuidValues {
                            eax: leaf,
                            ebx: subleaf,
                            ..Default::default()
                        }
                    )
                    .unwrap(),
                    None
                );
            }
        }

        // One entry for each of leaves 0-2 and three entries each for 3-5.
        let mut len = map.len();
        assert_eq!(len, 3 + (3 * 3));

        // Manually remove all of leaf 5's subleaves.
        for subleaf in 6..9 {
            assert_eq!(
                map.remove(CpuidIdent::subleaf(5, subleaf)).unwrap(),
                CpuidValues { eax: 5, ebx: subleaf, ..Default::default() }
            );

            len -= 1;
            assert_eq!(map.len(), len);
        }

        // Leaf 5 should no longer be in the map in any of its guises.
        assert_eq!(map.get(CpuidIdent::leaf(5)), None);
        for subleaf in 6..9 {
            assert_eq!(map.get(CpuidIdent::subleaf(5, subleaf)), None);
        }

        // Removing leaves 3 and 4 without specifying their subleaves should
        // fail.
        assert_eq!(map.remove(CpuidIdent::leaf(3)), None);
        assert_eq!(map.remove(CpuidIdent::leaf(4)), None);
        assert_eq!(map.len(), len);

        // Remove leaf 3 and its subleaves via `remove_leaf`.
        map.remove_leaf(3);
        len -= 3;
        assert_eq!(map.len(), len);

        // Remove leaf 4 via `retain`.
        map.retain(|id, _val| id.leaf != 4);
        len -= 3;
        assert_eq!(map.len(), len);

        // Removing leaf 0, subleaf 0 should fail.
        assert_eq!(map.remove(CpuidIdent::subleaf(0, 0)), None);

        // Remove leaves 0-2 by their IDs.
        for leaf in 0..3 {
            assert!(map.remove(CpuidIdent::leaf(leaf)).is_some());
            len -= 1;
            assert_eq!(map.len(), len);
        }

        assert_eq!(len, 0);
        assert!(map.is_empty());
    }

    #[derive(Debug)]
    enum MapEntry {
        Leaf(u32, CpuidValues),
        Subleaves(u32, Vec<(u32, CpuidValues)>),
    }

    /// Produces a random CPUID leaf entry. Each entry has an leaf number in
    /// [0..8) and one of the following values:
    ///
    /// - One of sixteen random [`CpuidValues`] values
    /// - One subleaf with index [0..5) and one of two values
    /// - Two such subleaves
    /// - Three such subleaves
    ///
    /// There are (16 + 10 + 100 + 1000) = 1,126 possible values for each leaf,
    /// for a total of 9,008 possible leaf entries.
    fn map_entry_strategy() -> impl Strategy<Value = MapEntry> {
        const MAX_LEAF: u32 = 8;
        prop_oneof![
            (0..MAX_LEAF, prop::array::uniform4(0..2u32)).prop_map(
                |(leaf, value_arr)| { MapEntry::Leaf(leaf, value_arr.into()) }
            ),
            (
                0..MAX_LEAF,
                prop::collection::vec(0..5u32, 1..=3),
                proptest::bool::ANY
            )
                .prop_map(|(leaf, subleaves, set_value)| {
                    let value = if set_value {
                        CpuidValues { eax: 1, ebx: 2, ecx: 3, edx: 4 }
                    } else {
                        CpuidValues::default()
                    };
                    let subleaves = subleaves
                        .into_iter()
                        .map(|subleaf| (subleaf, value))
                        .collect();
                    MapEntry::Subleaves(leaf, subleaves)
                })
        ]
    }

    proptest! {
        /// Verifies that a [`CpuidMapIterator`] visits all of the leaf and
        /// subleaf entries in a map and does so in the expected order.
        ///
        /// proptest will generate a set of 3-8 leaves for each test according
        /// to the strategy defined in [`map_entry_strategy`].
        #[test]
        fn map_iteration_order(
            entries in prop::collection::vec(map_entry_strategy(), 3..=8)
        ) {
            let mut map = CpuidMap::default();
            let mut _expected_len = 0;

            // Insert all of the entries into the map. The input array may have
            // some duplicates and may assign both a no-subleaf and a subleaf
            // value to a single leaf; ignore all of the resulting errors and
            // substitutions.
            for entry in entries {
                match entry {
                    MapEntry::Leaf(leaf, values) => {
                        if let Ok(None) = map.insert(
                            CpuidIdent::leaf(leaf),
                            values
                        ) {
                            _expected_len += 1;
                        }
                    }
                    MapEntry::Subleaves(leaf, subleaves) => {
                        for (subleaf, values) in subleaves {
                            if let Ok(None) = map.insert(
                                CpuidIdent::subleaf(leaf, subleaf),
                                values
                            ) {
                                _expected_len += 1;
                            }
                        }
                    }
                }
            }

            assert_eq!(map.len(), _expected_len);

            // The iterator should visit leaves in order and return subleaves
            // before the next leaf. This happens to be the ordering provided by
            // `CpuidIdent`'s `Ord` implementation, so it suffices just to
            // compare identifiers directly.
            let mut _observed_len = 0;
            let output: Vec<(CpuidIdent, CpuidValues)> = map.iter().collect();
            for (first, second) in
                output.as_slice().windows(2).map(|sl| (sl[0].0, sl[1].0)) {
                assert!(first < second, "first: {first:?}, second: {second:?}");
                _observed_len += 1;
            }

            // The `windows(2)` iterator will not count the last entry (it has
            // no successor), so the actual observed length is one more than the
            // number of observed iterations. (Note that by construction the map
            // is not empty, so there is always a last entry.)
            assert_eq!(_observed_len + 1, _expected_len);
        }
    }
}
