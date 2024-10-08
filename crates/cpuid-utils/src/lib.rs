// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility functions and types for working with CPUID values.
//!
//! If this crate is built with the `instance-spec` feature, this module
//! includes mechanisms for converting from instance spec CPUID entries to and
//! from the CPUID map types in the crate. This is feature-gated so that the
//! main Propolis lib can use this library without depending on
//! `propolis-api-types`.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::RangeInclusive,
};

pub use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor};
use thiserror::Error;

pub mod bits;

#[cfg(feature = "instance-spec")]
mod instance_spec;

#[cfg(feature = "instance-spec")]
pub use instance_spec::*;

/// A map from CPUID leaves to CPUID values.
#[derive(Clone, Debug, Default)]
pub struct CpuidMap(pub BTreeMap<CpuidIdent, CpuidValues>);

#[derive(Debug, Error)]
pub enum CpuidMapMismatch {
    #[error("CPUID leaves not found in both sets ({0:?})")]
    MismatchedLeaves(Vec<CpuidIdent>),

    #[error("CPUID leaves have different values in different sets ({0:?})")]
    MismatchedValues(Vec<(CpuidIdent, CpuidValues, CpuidValues)>),
}

impl CpuidMap {
    /// Returns `Ok` if `self` is equivalent to `other`; if not, returns a
    /// [`CpuidMapMismatch`] describing the difference between the maps.
    pub fn is_equivalent(&self, other: &Self) -> Result<(), CpuidMapMismatch> {
        let this_set: BTreeSet<_> = self.0.keys().collect();
        let other_set: BTreeSet<_> = other.0.keys().collect();
        let diff = this_set.symmetric_difference(&other_set);
        let diff: Vec<CpuidIdent> = diff.copied().copied().collect();

        if !diff.is_empty() {
            return Err(CpuidMapMismatch::MismatchedLeaves(diff));
        }

        let mut mismatches = vec![];
        for (this_leaf, this_value) in self.0.iter() {
            let other_value = other
                .0
                .get(this_leaf)
                .expect("key sets were already found to be equal");

            if this_value != other_value {
                mismatches.push((*this_leaf, *this_value, *other_value));
            }
        }

        if !mismatches.is_empty() {
            Err(CpuidMapMismatch::MismatchedValues(mismatches))
        } else {
            Ok(())
        }
    }
}

/// The range of standard, architecturally-defined CPUID leaves.
pub const STANDARD_LEAVES: RangeInclusive<u32> = 0..=0xFFFF;

/// The range of extended CPUID leaves. The meanings of these leaves are CPU
/// vendor-specific.
pub const EXTENDED_LEAVES: RangeInclusive<u32> = 0x8000_0000..=0x8000_FFFF;

/// Queries the supplied CPUID leaf on the caller's machine.
#[cfg(target_arch = "x86_64")]
pub fn host_query(leaf: CpuidIdent) -> CpuidValues {
    unsafe {
        core::arch::x86_64::__cpuid_count(leaf.leaf, leaf.subleaf.unwrap_or(0))
    }
    .into()
}

#[cfg(not(target_arch = "x86_64"))]
pub fn host_query(leaf: CpuidIdent) -> CpuidValues {
    panic!("host CPUID queries only work on x86-64 hosts")
}

/// Queries subleaf 0 of all of the valid CPUID leaves on the host and returns
/// the results in a [`CpuidMap`].
///
/// # Panics
///
/// Panics if the target architecture is not x86-64.
pub fn host_query_all() -> CpuidMap {
    let mut map = BTreeMap::new();
    let leaf_0 = CpuidIdent::leaf(*STANDARD_LEAVES.start());
    let leaf_0_values = host_query(leaf_0);
    map.insert(leaf_0, leaf_0_values);

    for l in (STANDARD_LEAVES.start() + 1)..=leaf_0_values.eax {
        let leaf = CpuidIdent::leaf(l);
        map.insert(leaf, host_query(leaf));
    }

    // This needs to be done by hand because the `__get_cpuid_max` intrinsic
    // only returns the maximum standard leaf.
    let ext_leaf_0 = CpuidIdent::leaf(*EXTENDED_LEAVES.start());
    let ext_leaf_0_values = host_query(ext_leaf_0);
    map.insert(ext_leaf_0, ext_leaf_0_values);
    for l in (EXTENDED_LEAVES.start() + 1)..=ext_leaf_0_values.eax {
        let leaf = CpuidIdent::leaf(l);
        map.insert(leaf, host_query(leaf));
    }

    CpuidMap(map)
}
