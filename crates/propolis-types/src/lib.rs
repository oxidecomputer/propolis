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
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

const PCI_DEVICES_PER_BUS: u8 = 32;
const PCI_FUNCTIONS_PER_DEVICE: u8 = 8;

/// A PCI bus/device/function tuple. Supports conversion from a string formatted
/// as "B.D.F", e.g. "0.7.0".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, JsonSchema)]
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

impl Serialize for PciPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(format!("{}", self).as_str())
    }
}

impl<'d> Deserialize<'d> for PciPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'d>,
    {
        let s = String::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod test {
    use super::PciPath;
    use serde::Deserialize;
    use serde_test::{assert_tokens, Token};
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

    #[test]
    fn pci_path_serialization() {
        for (input, expected) in TEST_CASES {
            match expected {
                Ok(path) => {
                    assert_tokens(path, &[Token::Str(input)]);
                }
                Err(_) => {
                    // Manually deserialize instead of using
                    // serde_test::assert_tokens_de_error to avoid having to
                    // specify exact error messages.
                    let tokens = [Token::Str(input)];
                    let mut de = serde_test::Deserializer::new(&tokens);
                    assert!(PciPath::deserialize(&mut de).is_err());
                }
            }
        }
    }
}
