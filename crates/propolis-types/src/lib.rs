// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fundamental types shared by other Propolis crates.
//!
//! This crate defines some basic types that are shared by multiple other
//! Propolis crates (library, client, server, and/or standalone) such that they
//! can all use those types (and implement their own conversions to/from them)
//! without any layering oddities.

use std::fmt::Display;
use std::io::{Error, ErrorKind};
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const PCI_DEVICES_PER_BUS: u8 = 32;
const PCI_FUNCTIONS_PER_DEVICE: u8 = 8;

/// A PCI bus/device/function tuple.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Debug,
    JsonSchema,
    Serialize,
    Deserialize,
)]
pub struct PciPath {
    bus: u8,
    device: u8,
    function: u8,
}

impl PciPath {
    pub fn new(
        bus: u8,
        device: u8,
        function: u8,
    ) -> Result<Self, std::io::Error> {
        if device >= PCI_DEVICES_PER_BUS {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "PCI device {} outside range of 0-{}",
                    device,
                    PCI_DEVICES_PER_BUS - 1
                ),
            ));
        }

        if function >= PCI_FUNCTIONS_PER_DEVICE {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "PCI function {} outside range of 0-{}",
                    function,
                    PCI_FUNCTIONS_PER_DEVICE - 1
                ),
            ));
        }

        Ok(Self { bus, device, function })
    }

    #[inline]
    pub fn bus(&self) -> u8 {
        self.bus
    }

    #[inline]
    pub fn device(&self) -> u8 {
        self.device
    }

    #[inline]
    pub fn function(&self) -> u8 {
        self.function
    }
}

impl FromStr for PciPath {
    type Err = std::io::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut fields = Vec::with_capacity(3);
        for f in s.split('.') {
            fields.push(u8::from_str(f).map_err(|e| {
                Self::Err::new(
                    ErrorKind::InvalidInput,
                    format!("Failed to parse PCI path {}: {}", s, e),
                )
            })?);
        }

        if fields.len() != 3 {
            return Err(Self::Err::new(
                ErrorKind::InvalidInput,
                format!(
                    "Expected 3 fields in PCI path {}, got {}",
                    s,
                    fields.len()
                ),
            ));
        }

        Self::new(fields[0], fields[1], fields[2])
    }
}

impl Display for PciPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.bus, self.device, self.function)
    }
}

#[cfg(test)]
mod test {
    use super::PciPath;
    use std::str::FromStr;

    const TEST_CASES: &[(&str, Result<PciPath, ()>)] = &[
        ("0.7.0", Ok(PciPath { bus: 0, device: 7, function: 0 })),
        ("1.2.3", Ok(PciPath { bus: 1, device: 2, function: 3 })),
        ("0.40.0", Err(())),
        ("0.1.9", Err(())),
        ("255.254.253", Err(())),
        ("1000.0.0", Err(())),
        ("4/3/4", Err(())),
        ("a.b.c", Err(())),
        ("1.5#4", Err(())),
        ("", Err(())),
        ("alas, poor PCI device", Err(())),
    ];

    #[test]
    fn pci_path_from_str() {
        for (input, expected) in TEST_CASES {
            match PciPath::from_str(input) {
                Ok(path) => assert_eq!(path, expected.unwrap()),
                Err(_) => assert!(
                    expected.is_err(),
                    "Expected error parsing PCI path {}",
                    input
                ),
            }
        }
    }
}

/// A CPUID leaf/subleaf (function/index) specifier.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Debug,
    JsonSchema,
    Serialize,
    Deserialize,
)]
pub struct CpuidLeaf {
    pub leaf: u32,
    pub subleaf: Option<u32>,
}

impl CpuidLeaf {
    pub fn leaf(leaf: u32) -> Self {
        Self { leaf, subleaf: None }
    }

    pub fn subleaf(leaf: u32, subleaf: u32) -> Self {
        Self { leaf, subleaf: Some(subleaf) }
    }
}

/// Values returned by a CPUID instruction.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Debug,
    JsonSchema,
    Serialize,
    Deserialize,
    Default,
)]
pub struct CpuidValues {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

impl From<[u32; 4]> for CpuidValues {
    fn from(value: [u32; 4]) -> Self {
        Self { eax: value[0], ebx: value[1], ecx: value[2], edx: value[3] }
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, Debug, JsonSchema, Serialize, Deserialize,
)]
pub enum CpuidVendor {
    Amd,
    Intel,
}

impl CpuidVendor {
    pub fn is_amd(self) -> bool {
        self == Self::Amd
    }

    pub fn is_intel(self) -> bool {
        self == Self::Intel
    }
}

impl TryFrom<CpuidValues> for CpuidVendor {
    type Error = &'static str;

    fn try_from(value: CpuidValues) -> Result<Self, Self::Error> {
        match (value.ebx, value.ecx, value.edx) {
            // AuthenticAmd
            (0x68747541, 0x444d4163, 0x69746e65) => Ok(Self::Amd),
            // GenuineIntel
            (0x756e6547, 0x6c65746e, 0x49656e69) => Ok(Self::Intel),
            _ => Err("unrecognized vendor"),
        }
    }
}
