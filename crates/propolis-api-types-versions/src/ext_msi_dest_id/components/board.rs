// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VM mainboard components for the `EXT_MSI_DEST_ID` API version.
//!
//! This version allows the bhyve guest hypervisor interface to carry a set of
//! optional features, and adds a new Hyper-V feature flag.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1;
use crate::v1::components::board::{Chipset, Cpuid};

/// Flags that enable optional bhyve enlightenments.
#[derive(
    Clone,
    Copy,
    Deserialize,
    Serialize,
    Debug,
    JsonSchema,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum BhyveFeatureFlag {
    /// Advertise support for 15-bit Extended Destination IDs in MSIs and I/O
    /// APIC Redirection Table Entries, allowing guests to address more than
    /// 255 vCPUs without interrupt remapping.
    ExtMsiDestId,
}

/// Flags that enable "simple" Hyper-V enlightenments that require no
/// feature-specific configuration.
//
// NOTE: This enum's variants should never have any associated data (note that
// the type doesn't use serde's `tag` and `content` attributes). If a future
// enlightenment requires associated data, it should be put into a
// `HyperVExtendedFeatures` struct (or similar), and the `HyperV` variant of
// `GuestHypervisorInterface` should be extended to `Option`ally include that
// struct.
#[derive(
    Clone,
    Copy,
    Deserialize,
    Serialize,
    Debug,
    JsonSchema,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum HyperVFeatureFlag {
    /// Expose the partition reference time MSR and the reference TSC page.
    ReferenceTsc,

    /// Advertise support for 15-bit Extended Destination IDs in MSIs and I/O
    /// APIC Redirection Table Entries, allowing guests to address more than
    /// 255 vCPUs without interrupt remapping.
    ExtendedIoapicRte,
}

/// A hypervisor interface to expose to the guest.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "type",
    content = "value"
)]
pub enum GuestHypervisorInterface {
    /// Expose a bhyve-like interface ("bhyve bhyve " as the hypervisor ID in
    /// leaf 0x4000_0000) with the supplied features enabled.
    Bhyve { features: BTreeSet<BhyveFeatureFlag> },

    /// Expose a Hyper-V-compatible hypervisor interface with the supplied
    /// features enabled.
    HyperV { features: BTreeSet<HyperVFeatureFlag> },
}

impl Default for GuestHypervisorInterface {
    fn default() -> Self {
        Self::Bhyve { features: BTreeSet::new() }
    }
}

impl GuestHypervisorInterface {
    pub(crate) fn is_default(&self) -> bool {
        matches!(self, Self::Bhyve { features } if features.is_empty())
    }
}

/// A VM's mainboard.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// The number of virtual logical processors attached to this VM.
    pub cpus: u8,

    /// The amount of guest RAM attached to this VM.
    pub memory_mb: u64,

    /// The chipset to expose to guest software.
    pub chipset: Chipset,

    /// The hypervisor platform to expose to the guest. The default is a
    /// bhyve-compatible interface with no additional features.
    #[serde(
        default,
        skip_serializing_if = "GuestHypervisorInterface::is_default"
    )]
    pub guest_hv_interface: GuestHypervisorInterface,

    /// The CPUID values to expose to the guest. If `None`, bhyve will derive
    /// default values from the host's CPUID values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpuid: Option<Cpuid>,
    // TODO: Processor and NUMA topology.
}

/// The error returned when a guest hypervisor interface uses features that
/// cannot be expressed in the v1 API types.
#[derive(thiserror::Error, Debug)]
#[error("cannot convert guest hypervisor interface to v1: {reason}")]
pub struct InvalidV1GuestHypervisorInterface {
    pub(crate) reason: &'static str,
}

impl From<v1::components::board::HyperVFeatureFlag> for HyperVFeatureFlag {
    fn from(old: v1::components::board::HyperVFeatureFlag) -> Self {
        match old {
            v1::components::board::HyperVFeatureFlag::ReferenceTsc => {
                Self::ReferenceTsc
            }
        }
    }
}

impl TryFrom<HyperVFeatureFlag> for v1::components::board::HyperVFeatureFlag {
    type Error = InvalidV1GuestHypervisorInterface;

    fn try_from(new: HyperVFeatureFlag) -> Result<Self, Self::Error> {
        match new {
            HyperVFeatureFlag::ReferenceTsc => Ok(Self::ReferenceTsc),
            HyperVFeatureFlag::ExtendedIoapicRte => {
                Err(InvalidV1GuestHypervisorInterface {
                    reason: "Hyper-V feature extended_ioapic_rte",
                })
            }
        }
    }
}

impl From<v1::components::board::GuestHypervisorInterface>
    for GuestHypervisorInterface
{
    fn from(old: v1::components::board::GuestHypervisorInterface) -> Self {
        match old {
            v1::components::board::GuestHypervisorInterface::Bhyve => {
                Self::Bhyve { features: BTreeSet::new() }
            }
            v1::components::board::GuestHypervisorInterface::HyperV {
                features,
            } => Self::HyperV {
                features: features.into_iter().map(Into::into).collect(),
            },
        }
    }
}

impl TryFrom<GuestHypervisorInterface>
    for v1::components::board::GuestHypervisorInterface
{
    type Error = InvalidV1GuestHypervisorInterface;

    fn try_from(new: GuestHypervisorInterface) -> Result<Self, Self::Error> {
        match new {
            GuestHypervisorInterface::Bhyve { features } => {
                if !features.is_empty() {
                    return Err(InvalidV1GuestHypervisorInterface {
                        reason: "bhyve interface with features",
                    });
                }
                Ok(Self::Bhyve)
            }
            GuestHypervisorInterface::HyperV { features } => Ok(Self::HyperV {
                features: features
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            }),
        }
    }
}

impl From<v1::components::board::Board> for Board {
    fn from(old: v1::components::board::Board) -> Self {
        let v1::components::board::Board {
            cpus,
            memory_mb,
            chipset,
            guest_hv_interface,
            cpuid,
        } = old;
        Self {
            cpus,
            memory_mb,
            chipset,
            guest_hv_interface: guest_hv_interface.into(),
            cpuid,
        }
    }
}

impl TryFrom<Board> for v1::components::board::Board {
    type Error = InvalidV1GuestHypervisorInterface;

    fn try_from(new: Board) -> Result<Self, Self::Error> {
        let Board { cpus, memory_mb, chipset, guest_hv_interface, cpuid } = new;
        Ok(Self {
            cpus,
            memory_mb,
            chipset,
            guest_hv_interface: guest_hv_interface.try_into()?,
            cpuid,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn default_interface_is_not_serialized() {
        let board = Board {
            cpus: 1,
            memory_mb: 1024,
            chipset: Chipset::default(),
            guest_hv_interface: GuestHypervisorInterface::default(),
            cpuid: None,
        };
        let json = serde_json::to_value(&board).unwrap();
        assert!(json.get("guest_hv_interface").is_none());

        let roundtrip: Board = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip.guest_hv_interface, board.guest_hv_interface);
    }

    #[test]
    fn bhyve_features_roundtrip() {
        let iface = GuestHypervisorInterface::Bhyve {
            features: [BhyveFeatureFlag::ExtMsiDestId].into(),
        };
        let json = serde_json::to_value(&iface).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "bhyve",
                "value": { "features": ["ext_msi_dest_id"] }
            })
        );
        let roundtrip: GuestHypervisorInterface =
            serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip, iface);
    }

    #[test]
    fn v1_downgrade_rejects_new_features() {
        let bhyve = GuestHypervisorInterface::Bhyve {
            features: [BhyveFeatureFlag::ExtMsiDestId].into(),
        };
        assert!(v1::components::board::GuestHypervisorInterface::try_from(
            bhyve
        )
        .is_err());

        let hyperv = GuestHypervisorInterface::HyperV {
            features: [
                HyperVFeatureFlag::ReferenceTsc,
                HyperVFeatureFlag::ExtendedIoapicRte,
            ]
            .into(),
        };
        assert!(v1::components::board::GuestHypervisorInterface::try_from(
            hyperv
        )
        .is_err());

        let hyperv_tsc = GuestHypervisorInterface::HyperV {
            features: [HyperVFeatureFlag::ReferenceTsc].into(),
        };
        let v1 = v1::components::board::GuestHypervisorInterface::try_from(
            hyperv_tsc.clone(),
        )
        .unwrap();
        assert_eq!(GuestHypervisorInterface::from(v1), hyperv_tsc);
    }
}
