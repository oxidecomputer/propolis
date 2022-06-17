//! Common definitions and helpers shared by all instance spec versions.

use std::io::ErrorKind;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A PCI bus/device/function tuple. Supports conversion from a string formatted
/// as "B.D.F", e.g. "0.7.0".
#[derive(
    Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug,
)]
pub struct PciPath(pub u8, pub u8, pub u8);

impl FromStr for PciPath {
    type Err = std::io::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut fields = Vec::with_capacity(3);
        for f in s.split('.') {
            fields.push(
                u8::from_str(f)
                    .map_err(|e| Self::Err::new(ErrorKind::InvalidInput, e))?,
            );
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

        Ok(PciPath(fields[0], fields[1], fields[2]))
    }
}

#[cfg(test)]
mod test {
    use super::PciPath;
    use std::str::FromStr;

    #[test]
    fn pci_path_conversions() {
        assert_eq!(PciPath::from_str("0.7.0").unwrap(), PciPath(0, 7, 0));
        assert_eq!(
            PciPath::from_str("255.254.253").unwrap(),
            PciPath(255, 254, 253)
        );
        assert!(PciPath::from_str("1000.0.0").is_err());
        assert!(PciPath::from_str("4/3/4").is_err());
        assert!(PciPath::from_str("a.b.c").is_err());
        assert!(PciPath::from_str("1.5#4").is_err());
        assert!(PciPath::from_str("").is_err());
        assert!(PciPath::from_str("alas, poor PCI device").is_err());
    }
}
