// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility functions and types for working with CPUID values.
//!
//! If this crate is built with the `instance-spec` feature, this module
//! includes mechanisms for converting from instance spec CPUID entries to and
//! from the CPUID map types in the crate. These conversions require that the
//! input list of CPUID entries contains no duplicate leaf/subleaf pairs and
//! that the specified leaves all fall in the standard or extended ranges.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::RangeInclusive,
};

pub use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor};
use thiserror::Error;

#[cfg(feature = "instance-spec")]
mod instance_spec;

#[cfg(feature = "instance-spec")]
pub use instance_spec::*;

/// A map from CPUID leaves to CPUID values.
#[derive(Clone, Debug, Default)]
pub struct CpuidMap(pub BTreeMap<CpuidIdent, CpuidValues>);

#[derive(Debug, Error)]
pub enum CpuidMapMismatch {
    #[error("mismatched CPUID leaves {0:?}")]
    MismatchedLeaves(Vec<CpuidIdent>),

    #[error("mismatched values for leaves {0:?}")]
    MismatchedValues(Vec<(CpuidIdent, CpuidValues, CpuidValues)>),
}

impl CpuidMap {
    /// Returns `Ok` if `self` is equivalent to `other`; if not, returns a
    /// [`CpuidMapMismatch`] describing the difference that caused the
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
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn host_query(leaf: CpuidIdent) -> CpuidValues {
    let mut res = CpuidValues::default();

    unsafe {
        std::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) res.ebx,
            // select cpuid 0, also specify eax as clobbered
            inout("eax") leaf.leaf => res.eax,
            inout("ecx") leaf.subleaf.unwrap_or(0) => res.ecx,
            out("edx") res.edx,
        );
    }

    res
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn host_query(leaf: CpuidIdent) -> CpuidValues {
    panic!("host CPUID queries only work on x86")
}

/// Queries subleaf 0 of all of the valid CPUID leaves on the host and returns
/// the results in a [`CpuidMap`].
///
/// # Panics
///
/// Panics if the target architecture is not x86 or x86-64.
pub fn host_query_all() -> CpuidMap {
    let mut map = BTreeMap::new();
    let leaf_0 = CpuidIdent::leaf(*STANDARD_LEAVES.start());
    let leaf_0_values = host_query(leaf_0);
    map.insert(leaf_0, leaf_0_values);

    for l in (STANDARD_LEAVES.start() + 1)..=leaf_0_values.eax {
        let leaf = CpuidIdent::leaf(l);
        map.insert(leaf, host_query(leaf));
    }

    let ext_leaf_0 = CpuidIdent::leaf(*EXTENDED_LEAVES.start());
    let ext_leaf_0_values = host_query(ext_leaf_0);
    map.insert(ext_leaf_0, ext_leaf_0_values);
    for l in (EXTENDED_LEAVES.start() + 1)..=ext_leaf_0_values.eax {
        let leaf = CpuidIdent::leaf(l);
        map.insert(leaf, host_query(leaf));
    }

    CpuidMap(map)
}
