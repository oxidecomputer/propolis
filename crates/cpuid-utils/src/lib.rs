// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Utility functions and types for working with CPUID values.

use std::{collections::BTreeMap, ops::RangeInclusive};

pub use propolis_types::{CpuidIdent, CpuidValues, CpuidVendor};

#[cfg(feature = "instance-spec")]
use propolis_api_types::instance_spec::components::board::CpuidEntry;

#[cfg(feature = "instance-spec")]
use thiserror::Error;

/// A map from CPUID leaves to CPUID values.
#[derive(Clone, Debug, Default)]
pub struct CpuidMap(pub BTreeMap<CpuidIdent, CpuidValues>);

#[cfg(feature = "instance-spec")]
#[derive(Debug, Error)]
pub enum CpuidMapConversionError {
    #[error("duplicate leaf and subleaf ({0:x}, {1:?})")]
    DuplicateLeaf(u32, Option<u32>),

    #[error("leaf {0:x} not in standard or extended range")]
    LeafOutOfRange(u32),
}

pub const STANDARD_LEAVES: RangeInclusive<u32> = 0..=0xFFFF;
pub const EXTENDED_LEAVES: RangeInclusive<u32> = 0x8000_0000..=0x8000_FFFF;

#[cfg(feature = "instance-spec")]
impl From<CpuidMap> for Vec<CpuidEntry> {
    fn from(value: CpuidMap) -> Self {
        value
            .0
            .into_iter()
            .map(
                |(
                    CpuidIdent { leaf, subleaf },
                    CpuidValues { eax, ebx, ecx, edx },
                )| CpuidEntry {
                    leaf,
                    subleaf,
                    eax,
                    ebx,
                    ecx,
                    edx,
                },
            )
            .collect()
    }
}

#[cfg(feature = "instance-spec")]
impl TryFrom<Vec<CpuidEntry>> for CpuidMap {
    type Error = CpuidMapConversionError;

    fn try_from(
        value: Vec<CpuidEntry>,
    ) -> Result<Self, CpuidMapConversionError> {
        let mut map = BTreeMap::new();
        for CpuidEntry { leaf, subleaf, eax, ebx, ecx, edx } in
            value.into_iter()
        {
            if !(STANDARD_LEAVES.contains(&leaf)
                || EXTENDED_LEAVES.contains(&leaf))
            {
                return Err(CpuidMapConversionError::LeafOutOfRange(leaf));
            }

            if map
                .insert(
                    CpuidIdent { leaf, subleaf },
                    CpuidValues { eax, ebx, ecx, edx },
                )
                .is_some()
            {
                return Err(CpuidMapConversionError::DuplicateLeaf(
                    leaf, subleaf,
                ));
            }
        }

        Ok(Self(map))
    }
}

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
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
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

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn host_query_all() -> CpuidMap {
    panic!("host CPUID queries only work on x86")
}
