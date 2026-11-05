// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::components::backends;
use crate::v1::components::board;
use crate::v1::components::devices as v1_devices;
use crate::v1::instance::{InstanceProperties, InstanceState};
use crate::v1::instance_spec::SpecKey;
use crate::v2::instance_spec::SmbiosType1Input;
use crate::v3;
use crate::v3::components::devices as v3_devices;
use crate::v3::instance_spec::Component as V3Component;

pub use super::components::devices::NvmeDisk;

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema, PartialEq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "component",
    rename_all = "snake_case"
)]
pub enum Component {
    VirtioDisk(v1_devices::VirtioDisk),
    NvmeDisk(NvmeDisk),
    VirtioNic(v1_devices::VirtioNic),
    SerialPort(v1_devices::SerialPort),
    PciPciBridge(v1_devices::PciPciBridge),
    QemuPvpanic(v1_devices::QemuPvpanic),
    BootSettings(v1_devices::BootSettings),
    VirtioSocket(v3_devices::VirtioSocket),
    SoftNpuPciPort(v1_devices::SoftNpuPciPort),
    SoftNpuPort(v1_devices::SoftNpuPort),
    SoftNpuP9(v1_devices::SoftNpuP9),
    P9fs(v1_devices::P9fs),
    MigrationFailureInjector(v1_devices::MigrationFailureInjector),
    CrucibleStorageBackend(backends::CrucibleStorageBackend),
    FileStorageBackend(backends::FileStorageBackend),
    BlobStorageBackend(backends::BlobStorageBackend),
    VirtioNetworkBackend(backends::VirtioNetworkBackend),
    DlpiNetworkBackend(backends::DlpiNetworkBackend),
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
pub struct InstanceSpec {
    pub board: board::Board,
    pub components: BTreeMap<SpecKey, Component>,
    pub smbios: Option<SmbiosType1Input>,
}

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
#[error("cannot convert component to v3: {reason}")]
pub struct InvalidV3Component {
    pub(crate) reason: &'static str,
}

impl TryFrom<Component> for V3Component {
    type Error = InvalidV3Component;

    fn try_from(value: Component) -> Result<Self, Self::Error> {
        Ok(match value {
            Component::VirtioDisk(c) => V3Component::VirtioDisk(c),
            Component::NvmeDisk(c) => V3Component::NvmeDisk(c.try_into()?),
            Component::VirtioNic(c) => V3Component::VirtioNic(c),
            Component::SerialPort(c) => V3Component::SerialPort(c),
            Component::PciPciBridge(c) => V3Component::PciPciBridge(c),
            Component::QemuPvpanic(c) => V3Component::QemuPvpanic(c),
            Component::BootSettings(c) => V3Component::BootSettings(c),
            Component::VirtioSocket(c) => V3Component::VirtioSocket(c),
            Component::SoftNpuPciPort(c) => V3Component::SoftNpuPciPort(c),
            Component::SoftNpuPort(c) => V3Component::SoftNpuPort(c),
            Component::SoftNpuP9(c) => V3Component::SoftNpuP9(c),
            Component::P9fs(c) => V3Component::P9fs(c),
            Component::MigrationFailureInjector(c) => {
                V3Component::MigrationFailureInjector(c)
            }
            Component::CrucibleStorageBackend(c) => {
                V3Component::CrucibleStorageBackend(c)
            }
            Component::FileStorageBackend(c) => {
                V3Component::FileStorageBackend(c)
            }
            Component::BlobStorageBackend(c) => {
                V3Component::BlobStorageBackend(c)
            }
            Component::VirtioNetworkBackend(c) => {
                V3Component::VirtioNetworkBackend(c)
            }
            Component::DlpiNetworkBackend(c) => {
                V3Component::DlpiNetworkBackend(c)
            }
        })
    }
}

impl TryFrom<InstanceSpec> for v3::instance_spec::InstanceSpec {
    type Error = InvalidV3Component;

    fn try_from(new: InstanceSpec) -> Result<Self, Self::Error> {
        let components: BTreeMap<_, _> = new
            .components
            .into_iter()
            .map(|(k, v)| V3Component::try_from(v).map(|c| (k, c)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        Ok(Self { board: new.board, components, smbios: new.smbios })
    }
}

impl From<V3Component> for Component {
    fn from(old: V3Component) -> Self {
        match old {
            V3Component::VirtioDisk(c) => Component::VirtioDisk(c),
            V3Component::VirtioSocket(c) => Component::VirtioSocket(c),
            V3Component::NvmeDisk(c) => Component::NvmeDisk(c.into()),
            V3Component::VirtioNic(c) => Component::VirtioNic(c),
            V3Component::SerialPort(c) => Component::SerialPort(c),
            V3Component::PciPciBridge(c) => Component::PciPciBridge(c),
            V3Component::QemuPvpanic(c) => Component::QemuPvpanic(c),
            V3Component::BootSettings(c) => Component::BootSettings(c),
            V3Component::SoftNpuPciPort(c) => Component::SoftNpuPciPort(c),
            V3Component::SoftNpuPort(c) => Component::SoftNpuPort(c),
            V3Component::SoftNpuP9(c) => Component::SoftNpuP9(c),
            V3Component::P9fs(c) => Component::P9fs(c),
            V3Component::MigrationFailureInjector(c) => {
                Component::MigrationFailureInjector(c)
            }
            V3Component::CrucibleStorageBackend(c) => {
                Component::CrucibleStorageBackend(c)
            }
            V3Component::FileStorageBackend(c) => {
                Component::FileStorageBackend(c)
            }
            V3Component::BlobStorageBackend(c) => {
                Component::BlobStorageBackend(c)
            }
            V3Component::VirtioNetworkBackend(c) => {
                Component::VirtioNetworkBackend(c)
            }
            V3Component::DlpiNetworkBackend(c) => {
                Component::DlpiNetworkBackend(c)
            }
        }
    }
}

impl TryFrom<InstanceSpecStatus> for v3::instance_spec::InstanceSpecStatus {
    type Error = InvalidV3Component;

    fn try_from(new: InstanceSpecStatus) -> Result<Self, Self::Error> {
        match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Ok(Self::WaitingForMigrationSource)
            }
            InstanceSpecStatus::Present(spec) => {
                let v3_spec: v3::instance_spec::InstanceSpec =
                    spec.try_into()?;
                Ok(Self::Present(v3_spec))
            }
        }
    }
}

impl TryFrom<InstanceSpecGetResponse>
    for v3::instance_spec::InstanceSpecGetResponse
{
    type Error = InvalidV3Component;

    fn try_from(new: InstanceSpecGetResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            properties: new.properties,
            state: new.state,
            spec: new.spec.try_into()?,
        })
    }
}

impl From<v3::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v3::instance_spec::InstanceSpec) -> Self {
        Self {
            board: old.board,
            components: old
                .components
                .into_iter()
                .map(|(k, v)| (k, Component::from(v)))
                .collect(),
            smbios: old.smbios,
        }
    }
}
