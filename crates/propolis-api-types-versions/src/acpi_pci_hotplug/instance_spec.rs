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
use crate::v3::components::devices as v3_devices;
use crate::v6;
use crate::v6::components::devices as v6_devices;
use crate::v6::instance_spec::Component as V6Component;

pub use super::components::devices::Acpi;

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema, PartialEq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "component",
    rename_all = "snake_case"
)]
pub enum Component {
    VirtioDisk(v1_devices::VirtioDisk),
    NvmeDisk(v6_devices::NvmeDisk),
    VirtioNic(v1_devices::VirtioNic),
    SerialPort(v1_devices::SerialPort),
    PciPciBridge(v1_devices::PciPciBridge),
    QemuPvpanic(v1_devices::QemuPvpanic),
    BootSettings(v1_devices::BootSettings),
    VirtioSocket(v3_devices::VirtioSocket),
    Acpi(Acpi),
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
#[error("no such v6 component: {0:?}")]
pub struct InvalidV6Component(Component);

impl TryFrom<Component> for V6Component {
    type Error = InvalidV6Component;

    fn try_from(value: Component) -> Result<Self, Self::Error> {
        Ok(match value {
            Component::VirtioDisk(c) => V6Component::VirtioDisk(c),
            Component::NvmeDisk(c) => V6Component::NvmeDisk(c),
            Component::VirtioNic(c) => V6Component::VirtioNic(c),
            Component::SerialPort(c) => V6Component::SerialPort(c),
            Component::PciPciBridge(c) => V6Component::PciPciBridge(c),
            Component::QemuPvpanic(c) => V6Component::QemuPvpanic(c),
            Component::BootSettings(c) => V6Component::BootSettings(c),
            Component::VirtioSocket(c) => V6Component::VirtioSocket(c),
            component @ Component::Acpi(_) => {
                return Err(InvalidV6Component(component))
            }
            Component::SoftNpuPciPort(c) => V6Component::SoftNpuPciPort(c),
            Component::SoftNpuPort(c) => V6Component::SoftNpuPort(c),
            Component::SoftNpuP9(c) => V6Component::SoftNpuP9(c),
            Component::P9fs(c) => V6Component::P9fs(c),
            Component::MigrationFailureInjector(c) => {
                V6Component::MigrationFailureInjector(c)
            }
            Component::CrucibleStorageBackend(c) => {
                V6Component::CrucibleStorageBackend(c)
            }
            Component::FileStorageBackend(c) => {
                V6Component::FileStorageBackend(c)
            }
            Component::BlobStorageBackend(c) => {
                V6Component::BlobStorageBackend(c)
            }
            Component::VirtioNetworkBackend(c) => {
                V6Component::VirtioNetworkBackend(c)
            }
            Component::DlpiNetworkBackend(c) => {
                V6Component::DlpiNetworkBackend(c)
            }
        })
    }
}

impl From<InstanceSpec> for v6::instance_spec::InstanceSpec {
    fn from(new: InstanceSpec) -> Self {
        Self {
            board: new.board,
            components: new
                .components
                .into_iter()
                .filter_map(|(k, v)| {
                    V6Component::try_from(v).ok().map(|c| (k, c))
                })
                .collect(),
            smbios: new.smbios,
        }
    }
}

impl From<V6Component> for Component {
    fn from(old: V6Component) -> Self {
        match old {
            V6Component::VirtioDisk(c) => Component::VirtioDisk(c),
            V6Component::VirtioSocket(c) => Component::VirtioSocket(c),
            V6Component::NvmeDisk(c) => Component::NvmeDisk(c),
            V6Component::VirtioNic(c) => Component::VirtioNic(c),
            V6Component::SerialPort(c) => Component::SerialPort(c),
            V6Component::PciPciBridge(c) => Component::PciPciBridge(c),
            V6Component::QemuPvpanic(c) => Component::QemuPvpanic(c),
            V6Component::BootSettings(c) => Component::BootSettings(c),
            V6Component::SoftNpuPciPort(c) => Component::SoftNpuPciPort(c),
            V6Component::SoftNpuPort(c) => Component::SoftNpuPort(c),
            V6Component::SoftNpuP9(c) => Component::SoftNpuP9(c),
            V6Component::P9fs(c) => Component::P9fs(c),
            V6Component::MigrationFailureInjector(c) => {
                Component::MigrationFailureInjector(c)
            }
            V6Component::CrucibleStorageBackend(c) => {
                Component::CrucibleStorageBackend(c)
            }
            V6Component::FileStorageBackend(c) => {
                Component::FileStorageBackend(c)
            }
            V6Component::BlobStorageBackend(c) => {
                Component::BlobStorageBackend(c)
            }
            V6Component::VirtioNetworkBackend(c) => {
                Component::VirtioNetworkBackend(c)
            }
            V6Component::DlpiNetworkBackend(c) => {
                Component::DlpiNetworkBackend(c)
            }
        }
    }
}

impl From<InstanceSpecStatus> for v6::instance_spec::InstanceSpecStatus {
    fn from(new: InstanceSpecStatus) -> Self {
        match new {
            InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            InstanceSpecStatus::Present(spec) => Self::Present(spec.into()),
        }
    }
}

impl From<InstanceSpecGetResponse>
    for v6::instance_spec::InstanceSpecGetResponse
{
    fn from(new: InstanceSpecGetResponse) -> Self {
        Self {
            properties: new.properties,
            state: new.state,
            spec: new.spec.into(),
        }
    }
}

impl From<v6::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v6::instance_spec::InstanceSpec) -> Self {
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
