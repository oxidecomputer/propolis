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
use crate::v1::instance_spec::Component as V1Component;
use crate::v1::instance_spec::SpecKey;
use crate::v2;
use crate::v2::instance_spec::SmbiosType1Input;

pub use super::components::devices::VirtioSocket;

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "component",
    rename_all = "snake_case"
)]
pub enum Component {
    VirtioDisk(v1_devices::VirtioDisk),
    NvmeDisk(v1_devices::NvmeDisk),
    VirtioNic(v1_devices::VirtioNic),
    SerialPort(v1_devices::SerialPort),
    PciPciBridge(v1_devices::PciPciBridge),
    QemuPvpanic(v1_devices::QemuPvpanic),
    BootSettings(v1_devices::BootSettings),
    VirtioSocket(VirtioSocket),
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
#[error("no such v1 component: {0:?}")]
pub struct InvalidV1Component(Component);

impl TryFrom<Component> for V1Component {
    type Error = InvalidV1Component;

    fn try_from(value: Component) -> Result<Self, Self::Error> {
        Ok(match value {
            Component::VirtioDisk(c) => V1Component::VirtioDisk(c),
            Component::NvmeDisk(c) => V1Component::NvmeDisk(c),
            Component::VirtioNic(c) => V1Component::VirtioNic(c),
            Component::SerialPort(c) => V1Component::SerialPort(c),
            Component::PciPciBridge(c) => V1Component::PciPciBridge(c),
            Component::QemuPvpanic(c) => V1Component::QemuPvpanic(c),
            Component::BootSettings(c) => V1Component::BootSettings(c),
            component @ Component::VirtioSocket(_) => {
                return Err(InvalidV1Component(component))
            }
            Component::SoftNpuPciPort(c) => V1Component::SoftNpuPciPort(c),
            Component::SoftNpuPort(c) => V1Component::SoftNpuPort(c),
            Component::SoftNpuP9(c) => V1Component::SoftNpuP9(c),
            Component::P9fs(c) => V1Component::P9fs(c),
            Component::MigrationFailureInjector(c) => {
                V1Component::MigrationFailureInjector(c)
            }
            Component::CrucibleStorageBackend(c) => {
                V1Component::CrucibleStorageBackend(c)
            }
            Component::FileStorageBackend(c) => {
                V1Component::FileStorageBackend(c)
            }
            Component::BlobStorageBackend(c) => {
                V1Component::BlobStorageBackend(c)
            }
            Component::VirtioNetworkBackend(c) => {
                V1Component::VirtioNetworkBackend(c)
            }
            Component::DlpiNetworkBackend(c) => {
                V1Component::DlpiNetworkBackend(c)
            }
        })
    }
}

impl From<InstanceSpec> for v2::instance_spec::InstanceSpec {
    fn from(new: InstanceSpec) -> Self {
        Self {
            board: new.board,
            components: new
                .components
                .into_iter()
                .filter_map(|(k, v)| {
                    V1Component::try_from(v).ok().map(|c| (k, c))
                })
                .collect(),
            smbios: new.smbios,
        }
    }
}

impl From<V1Component> for Component {
    fn from(old: V1Component) -> Self {
        match old {
            V1Component::VirtioDisk(c) => Component::VirtioDisk(c),
            V1Component::NvmeDisk(c) => Component::NvmeDisk(c),
            V1Component::VirtioNic(c) => Component::VirtioNic(c),
            V1Component::SerialPort(c) => Component::SerialPort(c),
            V1Component::PciPciBridge(c) => Component::PciPciBridge(c),
            V1Component::QemuPvpanic(c) => Component::QemuPvpanic(c),
            V1Component::BootSettings(c) => Component::BootSettings(c),
            V1Component::SoftNpuPciPort(c) => Component::SoftNpuPciPort(c),
            V1Component::SoftNpuPort(c) => Component::SoftNpuPort(c),
            V1Component::SoftNpuP9(c) => Component::SoftNpuP9(c),
            V1Component::P9fs(c) => Component::P9fs(c),
            V1Component::MigrationFailureInjector(c) => {
                Component::MigrationFailureInjector(c)
            }
            V1Component::CrucibleStorageBackend(c) => {
                Component::CrucibleStorageBackend(c)
            }
            V1Component::FileStorageBackend(c) => {
                Component::FileStorageBackend(c)
            }
            V1Component::BlobStorageBackend(c) => {
                Component::BlobStorageBackend(c)
            }
            V1Component::VirtioNetworkBackend(c) => {
                Component::VirtioNetworkBackend(c)
            }
            V1Component::DlpiNetworkBackend(c) => {
                Component::DlpiNetworkBackend(c)
            }
        }
    }
}

impl From<InstanceSpecStatus> for v2::instance_spec::InstanceSpecStatus {
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
    for v2::instance_spec::InstanceSpecGetResponse
{
    fn from(new: InstanceSpecGetResponse) -> Self {
        Self {
            properties: new.properties,
            state: new.state,
            spec: new.spec.into(),
        }
    }
}

impl From<v2::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v2::instance_spec::InstanceSpec) -> Self {
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
