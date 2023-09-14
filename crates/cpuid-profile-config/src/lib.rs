// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Copy, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CpuVendor {
    Amd,
    Intel,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct CpuidProfile {
    pub vendor: CpuVendor,
    #[serde(flatten, default)]
    pub leaf: BTreeMap<String, toml::Value>,
}

/// `cpuid` entry parsed from a configured profile
#[derive(Copy, Clone)]
pub struct CpuidEntry {
    /// Function (eax) to match for `cpuid` leaf
    pub func: u32,
    /// Index (ecx) to (optionally) match for `cpuid` leaf
    pub idx: Option<u32>,

    /// Values (eax, ebx, ecx, edx) for `cpuid` leaf
    pub values: [u32; 4],
}

#[derive(Debug, thiserror::Error)]
pub enum CpuidParseError {
    #[error("Unable to parse leaf {0}: {1}")]
    Leaf(String, std::num::ParseIntError),
    #[error("Unable to parse values: {0}")]
    Values(&'static str),
}

impl TryFrom<&CpuidProfile> for Vec<CpuidEntry> {
    type Error = CpuidParseError;

    fn try_from(value: &CpuidProfile) -> Result<Self, Self::Error> {
        let mut entries = Vec::with_capacity(value.leaf.len());

        for (leaf, values) in value.leaf.iter() {
            let (func, idx) = match leaf.split_once('-') {
                None => (
                    u32::from_str_radix(leaf, 16)
                        .map_err(|e| CpuidParseError::Leaf(leaf.clone(), e))?,
                    None,
                ),
                Some((func_part, idx_part)) => (
                    u32::from_str_radix(func_part, 16)
                        .map_err(|e| CpuidParseError::Leaf(leaf.clone(), e))?,
                    Some(
                        u32::from_str_radix(idx_part, 16).map_err(|e| {
                            CpuidParseError::Leaf(leaf.clone(), e)
                        })?,
                    ),
                ),
            };
            let raw_regs = values
                .as_array()
                .ok_or(CpuidParseError::Values("expected array of values"))?;
            if raw_regs.len() != 4 {
                return Err(CpuidParseError::Values("expected 4 cpuid values"));
            }
            let mut values = [0u32; 4];
            for (v, raw) in values.iter_mut().zip(raw_regs.iter()) {
                let num = raw.as_integer().ok_or(CpuidParseError::Values(
                    "leaf values must be numeric",
                ))?;
                *v = u32::try_from(num).map_err(|_e| {
                    CpuidParseError::Values("leaf values must be valid u32")
                })?;
            }
            entries.push(CpuidEntry { func, idx, values });
        }
        Ok(entries)
    }
}
