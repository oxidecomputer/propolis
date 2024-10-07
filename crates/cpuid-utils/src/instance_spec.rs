// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers for converting instance spec CPUID entries into this module's types.

use super::*;

use propolis_api_types::instance_spec::components::board::CpuidEntry;
use thiserror::Error;

/// An error that can occur when converting a list of CPUID entries in an
/// instance spec into a [`CpuidMap`].
#[derive(Debug, Error)]
pub enum CpuidMapConversionError {
    #[error("duplicate leaf and subleaf ({0:x}, {1:?})")]
    DuplicateLeaf(u32, Option<u32>),

    #[error("leaf {0:x} specified subleaf 0 both explicitly and as None")]
    SubleafZeroAliased(u32),

    #[error("leaf {0:x} not in standard or extended range")]
    LeafOutOfRange(u32),
}

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

            if subleaf.is_none()
                && map.contains_key(&CpuidIdent::leaf_subleaf(leaf, 0))
            {
                return Err(CpuidMapConversionError::SubleafZeroAliased(leaf));
            }

            if let Some(0) = subleaf {
                if map.contains_key(&CpuidIdent::leaf(leaf)) {
                    return Err(CpuidMapConversionError::SubleafZeroAliased(
                        leaf,
                    ));
                }
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn subleaf_aliasing_forbidden() {
        let entries = vec![
            CpuidEntry {
                leaf: 0,
                subleaf: None,
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
            CpuidEntry {
                leaf: 0,
                subleaf: Some(0),
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
        ];

        assert!(CpuidMap::try_from(entries).is_err());
    }
}
