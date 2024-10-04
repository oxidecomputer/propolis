// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers for converting instance spec CPUID entries into this module's types.

use super::*;

use propolis_api_types::instance_spec::components::board::{Cpuid, CpuidEntry};

impl super::CpuidSet {
    pub fn into_instance_spec_cpuid(self) -> Cpuid {
        Cpuid { entries: self.map.into(), vendor: self.vendor }
    }
}

impl From<CpuidMap> for Vec<CpuidEntry> {
    fn from(value: CpuidMap) -> Self {
        value
            .iter()
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

    /// Converts a set of [`CpuidEntry`] structures from an instance spec into a
    /// [`CpuidMap`]. This conversion fails if
    ///
    /// - one or more of the entries' leaves is not in the standard or extended
    ///   ranges (0x0-0xFFFF and 0x80000000-0x8000FFFF),
    /// - a leaf/subleaf pair is specified more than once, or
    /// - two input entries specify the same leaf value, one specifies a subleaf
    ///   of `None`, and one specifies a subleaf of `Some`.
    fn try_from(
        value: Vec<CpuidEntry>,
    ) -> Result<Self, CpuidMapConversionError> {
        let mut map = Self::default();
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
                )?
                .is_some()
            {
                return Err(CpuidMapConversionError::DuplicateLeaf(
                    leaf, subleaf,
                ));
            }
        }

        Ok(map)
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
