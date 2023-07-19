//! VM mainboard components. Every VM has a board, even if it has no other
//! peripherals.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::instance_spec::migration::MigrationElement;

/// An Intel 440FX-compatible chipset.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct I440Fx {
    /// Specifies whether the chipset should allow PCI configuration space
    /// to be accessed through the PCIe extended configuration mechanism.
    pub enable_pcie: bool,
}

impl MigrationElement for I440Fx {
    fn kind(&self) -> &'static str {
        "i440fx"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.enable_pcie != other.enable_pcie {
            Err(MigrationCompatibilityError::PcieMismatch(
                self.enable_pcie,
                other.enable_pcie,
            )
            .into())
        } else {
            Ok(())
        }
    }
}

/// A kind of virtual chipset.
#[derive(
    Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema,
)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "type",
    content = "value"
)]
pub enum Chipset {
    /// An Intel 440FX-compatible chipset.
    I440Fx(I440Fx),
}

impl MigrationElement for Chipset {
    fn kind(&self) -> &'static str {
        match self {
            Self::I440Fx(_) => "i440fx",
        }
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        let (Self::I440Fx(this), Self::I440Fx(other)) = (self, other);
        this.can_migrate_from_element(other)
    }
}

/// A VM's mainboard.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// The number of virtual logical processors attached to this VM.
    pub cpus: u8,

    /// The amount of guest RAM attached to this VM.
    pub memory_mb: u64,

    /// The chipset to expose to guest software.
    pub chipset: Chipset,
    // TODO: Guest platform and CPU feature identification.
    // TODO: NUMA topology.
}

impl Default for Board {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
        }
    }
}

impl MigrationElement for Board {
    fn kind(&self) -> &'static str {
        "Board"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.cpus != other.cpus {
            Err(MigrationCompatibilityError::CpuCount(self.cpus, other.cpus)
                .into())
        } else if self.memory_mb != other.memory_mb {
            Err(MigrationCompatibilityError::MemorySize(
                self.memory_mb,
                other.memory_mb,
            )
            .into())
        } else if let Err(e) =
            self.chipset.can_migrate_from_element(&other.chipset)
        {
            Err(e.into())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Error)]
pub enum MigrationCompatibilityError {
    #[error("Boards have different CPU counts (self: {0}, other: {1})")]
    CpuCount(u8, u8),

    #[error("Boards have different memory amounts (self: {0}, other: {1})")]
    MemorySize(u64, u64),

    #[error("Chipsets have different PCIe settings (self: {0}, other: {1})")]
    PcieMismatch(bool, bool),
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn compatible_boards() {
        let b1 = Board {
            cpus: 8,
            memory_mb: 8192,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
        };

        assert!(b1.can_migrate_from_element(&b1).is_ok());
    }

    #[test]
    fn incompatible_boards() {
        let b1 = Board {
            cpus: 4,
            memory_mb: 4096,
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: true }),
        };

        let b2 = Board { cpus: 8, ..b1 };
        assert!(b1.can_migrate_from_element(&b2).is_err());

        let b2 = Board { memory_mb: b1.memory_mb * 2, ..b1 };
        assert!(b1.can_migrate_from_element(&b2).is_err());

        let b2 = Board {
            chipset: Chipset::I440Fx(I440Fx { enable_pcie: false }),
            ..b1
        };
        assert!(b1.can_migrate_from_element(&b2).is_err());
    }
}
