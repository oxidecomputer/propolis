// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specification types for the SMBIOS_SKU_FAMILY API version.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::components::board;
use crate::v1::instance::{InstanceProperties, InstanceState};
use crate::v1::instance_spec::SpecKey;
use crate::v2;
use crate::v6;
use crate::v6::instance_spec::Component;

/// Input for the guest SMBIOS type 1 (System Information) table, defined in
/// section 7.2 of the SMBIOS spec (DSP0134): <https://www.dmtf.org/standards/smbios>
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SmbiosType1Input {
    pub manufacturer: String,
    pub product_name: String,
    pub serial_number: String,
    pub version: u64,

    /// The SKU Number string in the emitted table. Unset means empty.
    #[serde(default)]
    pub sku_number: Option<String>,

    /// The Family string in the emitted table. Unset means empty.
    #[serde(default)]
    pub family: Option<String>,
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
pub struct InstanceSpec {
    pub board: board::Board,
    pub components: BTreeMap<SpecKey, Component>,
    pub smbios: Option<SmbiosType1Input>,
}

// The widened SmbiosType1Input pushes InstanceSpec past clippy's variant
// size threshold; boxing would ripple through every constructor for no gain.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value")]
pub enum InstanceSpecStatus {
    WaitingForMigrationSource,
    Present(InstanceSpec),
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecGetResponse {
    pub properties: InstanceProperties,
    pub state: InstanceState,
    pub spec: InstanceSpecStatus,
}

#[derive(thiserror::Error, Debug)]
#[error("SMBIOS type 1 input cannot be downgraded: {reason}")]
pub struct SmbiosDowngradeError {
    pub(crate) reason: &'static str,
}

impl From<v2::instance_spec::SmbiosType1Input> for SmbiosType1Input {
    fn from(old: v2::instance_spec::SmbiosType1Input) -> Self {
        let v2::instance_spec::SmbiosType1Input {
            manufacturer,
            product_name,
            serial_number,
            version,
        } = old;

        Self {
            manufacturer,
            product_name,
            serial_number,
            version,
            sku_number: None,
            family: None,
        }
    }
}

impl TryFrom<SmbiosType1Input> for v2::instance_spec::SmbiosType1Input {
    type Error = SmbiosDowngradeError;

    fn try_from(new: SmbiosType1Input) -> Result<Self, Self::Error> {
        let SmbiosType1Input {
            manufacturer,
            product_name,
            serial_number,
            version,
            sku_number,
            family,
        } = new;

        if sku_number.is_some() {
            return Err(SmbiosDowngradeError {
                reason: "SKU number cannot be downgraded",
            });
        }

        if family.is_some() {
            return Err(SmbiosDowngradeError {
                reason: "family cannot be downgraded",
            });
        }

        Ok(Self { manufacturer, product_name, serial_number, version })
    }
}

impl From<v6::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v6::instance_spec::InstanceSpec) -> Self {
        Self {
            board: old.board,
            components: old.components,
            smbios: old.smbios.map(Into::into),
        }
    }
}

impl TryFrom<InstanceSpec> for v6::instance_spec::InstanceSpec {
    type Error = SmbiosDowngradeError;

    fn try_from(new: InstanceSpec) -> Result<Self, Self::Error> {
        Ok(Self {
            board: new.board,
            components: new.components,
            smbios: new.smbios.map(TryInto::try_into).transpose()?,
        })
    }
}

impl TryFrom<InstanceSpecStatus> for v6::instance_spec::InstanceSpecStatus {
    type Error = SmbiosDowngradeError;

    fn try_from(new: InstanceSpecStatus) -> Result<Self, Self::Error> {
        Ok(match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            InstanceSpecStatus::Present(spec) => {
                Self::Present(spec.try_into()?)
            }
        })
    }
}

impl TryFrom<InstanceSpecGetResponse>
    for v6::instance_spec::InstanceSpecGetResponse
{
    type Error = SmbiosDowngradeError;

    fn try_from(new: InstanceSpecGetResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            properties: new.properties,
            state: new.state,
            spec: new.spec.try_into()?,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Verifies that upgrading a pre-v7 SMBIOS type 1 input leaves the new
    // fields unset.
    #[test]
    fn old_smbios_input_upgrades_with_new_fields_unset() {
        let old = v2::instance_spec::SmbiosType1Input {
            manufacturer: "Oxide".to_string(),
            product_name: "OxVM".to_string(),
            serial_number: "12345".to_string(),
            version: 3,
        };

        let new = SmbiosType1Input::from(old);
        assert_eq!(new.manufacturer, "Oxide");
        assert_eq!(new.product_name, "OxVM");
        assert_eq!(new.serial_number, "12345");
        assert_eq!(new.version, 3);
        assert_eq!(new.sku_number, None);
        assert_eq!(new.family, None);
    }

    // Verifies that the new fields block downgrading the input to v2 form
    // when set and downgrade cleanly when unset.
    #[test]
    fn new_smbios_fields_gate_downgrade() {
        let mut new = SmbiosType1Input {
            manufacturer: "Oxide".to_string(),
            product_name: "OxVM".to_string(),
            serial_number: "12345".to_string(),
            version: 3,
            sku_number: Some("913-0000019".to_string()),
            family: None,
        };

        assert!(
            v2::instance_spec::SmbiosType1Input::try_from(new.clone()).is_err()
        );

        new.sku_number = None;
        new.family = Some("Gimlet".to_string());
        assert!(
            v2::instance_spec::SmbiosType1Input::try_from(new.clone()).is_err()
        );

        new.family = None;
        let old = v2::instance_spec::SmbiosType1Input::try_from(new).unwrap();
        assert_eq!(old.manufacturer, "Oxide");
        assert_eq!(old.product_name, "OxVM");
        assert_eq!(old.serial_number, "12345");
        assert_eq!(old.version, 3);
    }

    // Verifies that a payload without the new fields still parses.
    #[test]
    fn smbios_input_serde_defaults() {
        let json = r#"{"manufacturer":"Oxide","product_name":"OxVM",
            "serial_number":"12345","version":3}"#;
        let input: SmbiosType1Input = serde_json::from_str(json).unwrap();
        assert_eq!(input.sku_number, None);
        assert_eq!(input.family, None);
    }
}
