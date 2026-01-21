// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specification types for the NVME_MODEL_NUMBER API version.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::components::devices;
use crate::v1;
use crate::v1::components::{backends, board};
use crate::v1::instance::{InstanceProperties, InstanceState};
use crate::v1::instance_spec::SpecKey;
use crate::v2;
use crate::v2::instance_spec::SmbiosType1Input;

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "component",
    rename_all = "snake_case"
)]
pub enum Component {
    VirtioDisk(v1::components::devices::VirtioDisk),
    NvmeDisk(devices::NvmeDisk),
    VirtioNic(v1::components::devices::VirtioNic),
    SerialPort(v1::components::devices::SerialPort),
    PciPciBridge(v1::components::devices::PciPciBridge),
    QemuPvpanic(v1::components::devices::QemuPvpanic),
    BootSettings(v1::components::devices::BootSettings),
    SoftNpuPciPort(v1::components::devices::SoftNpuPciPort),
    SoftNpuPort(v1::components::devices::SoftNpuPort),
    SoftNpuP9(v1::components::devices::SoftNpuP9),
    P9fs(v1::components::devices::P9fs),
    MigrationFailureInjector(v1::components::devices::MigrationFailureInjector),
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

// Conversions from v1 Component to v3 Component.
impl From<v1::instance_spec::Component> for Component {
    fn from(old: v1::instance_spec::Component) -> Self {
        match old {
            v1::instance_spec::Component::VirtioDisk(d) => Self::VirtioDisk(d),
            v1::instance_spec::Component::NvmeDisk(d) => {
                Self::NvmeDisk(d.into())
            }
            v1::instance_spec::Component::VirtioNic(n) => Self::VirtioNic(n),
            v1::instance_spec::Component::SerialPort(s) => Self::SerialPort(s),
            v1::instance_spec::Component::PciPciBridge(b) => {
                Self::PciPciBridge(b)
            }
            v1::instance_spec::Component::QemuPvpanic(p) => {
                Self::QemuPvpanic(p)
            }
            v1::instance_spec::Component::BootSettings(b) => {
                Self::BootSettings(b)
            }
            v1::instance_spec::Component::SoftNpuPciPort(p) => {
                Self::SoftNpuPciPort(p)
            }
            v1::instance_spec::Component::SoftNpuPort(p) => {
                Self::SoftNpuPort(p)
            }
            v1::instance_spec::Component::SoftNpuP9(p) => Self::SoftNpuP9(p),
            v1::instance_spec::Component::P9fs(p) => Self::P9fs(p),
            v1::instance_spec::Component::MigrationFailureInjector(m) => {
                Self::MigrationFailureInjector(m)
            }
            v1::instance_spec::Component::CrucibleStorageBackend(c) => {
                Self::CrucibleStorageBackend(c)
            }
            v1::instance_spec::Component::FileStorageBackend(f) => {
                Self::FileStorageBackend(f)
            }
            v1::instance_spec::Component::BlobStorageBackend(b) => {
                Self::BlobStorageBackend(b)
            }
            v1::instance_spec::Component::VirtioNetworkBackend(v) => {
                Self::VirtioNetworkBackend(v)
            }
            v1::instance_spec::Component::DlpiNetworkBackend(d) => {
                Self::DlpiNetworkBackend(d)
            }
        }
    }
}

// Conversions from v3 Component to v1 Component.
impl From<Component> for v1::instance_spec::Component {
    fn from(new: Component) -> Self {
        match new {
            Component::VirtioDisk(d) => Self::VirtioDisk(d),
            Component::NvmeDisk(d) => Self::NvmeDisk(d.into()),
            Component::VirtioNic(n) => Self::VirtioNic(n),
            Component::SerialPort(s) => Self::SerialPort(s),
            Component::PciPciBridge(b) => Self::PciPciBridge(b),
            Component::QemuPvpanic(p) => Self::QemuPvpanic(p),
            Component::BootSettings(b) => Self::BootSettings(b),
            Component::SoftNpuPciPort(p) => Self::SoftNpuPciPort(p),
            Component::SoftNpuPort(p) => Self::SoftNpuPort(p),
            Component::SoftNpuP9(p) => Self::SoftNpuP9(p),
            Component::P9fs(p) => Self::P9fs(p),
            Component::MigrationFailureInjector(m) => {
                Self::MigrationFailureInjector(m)
            }
            Component::CrucibleStorageBackend(c) => {
                Self::CrucibleStorageBackend(c)
            }
            Component::FileStorageBackend(f) => Self::FileStorageBackend(f),
            Component::BlobStorageBackend(b) => Self::BlobStorageBackend(b),
            Component::VirtioNetworkBackend(v) => Self::VirtioNetworkBackend(v),
            Component::DlpiNetworkBackend(d) => Self::DlpiNetworkBackend(d),
        }
    }
}

// Conversions from v1 InstanceSpec to v3 InstanceSpec.
impl From<v1::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v1::instance_spec::InstanceSpec) -> Self {
        Self {
            board: old.board,
            components: old
                .components
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            smbios: None,
        }
    }
}

// Conversions from v2 InstanceSpec to v3 InstanceSpec.
impl From<v2::instance_spec::InstanceSpec> for InstanceSpec {
    fn from(old: v2::instance_spec::InstanceSpec) -> Self {
        Self {
            board: old.board,
            components: old
                .components
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            smbios: old.smbios,
        }
    }
}

// Conversions from v3 InstanceSpec to v1 InstanceSpec.
impl From<InstanceSpec> for v1::instance_spec::InstanceSpec {
    fn from(new: InstanceSpec) -> Self {
        Self {
            board: new.board,
            components: new
                .components
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

// Conversions from v3 InstanceSpec to v2 InstanceSpec.
impl From<InstanceSpec> for v2::instance_spec::InstanceSpec {
    fn from(new: InstanceSpec) -> Self {
        Self {
            board: new.board,
            components: new
                .components
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            smbios: new.smbios,
        }
    }
}

// Conversions for InstanceSpecStatus.
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

impl From<v2::instance_spec::InstanceSpecStatus> for InstanceSpecStatus {
    fn from(old: v2::instance_spec::InstanceSpecStatus) -> Self {
        match old {
            v2::instance_spec::InstanceSpecStatus::WaitingForMigrationSource => {
                Self::WaitingForMigrationSource
            }
            v2::instance_spec::InstanceSpecStatus::Present(spec) => {
                Self::Present(spec.into())
            }
        }
    }
}

// Conversions for InstanceSpecGetResponse.
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

impl From<v2::instance_spec::InstanceSpecGetResponse>
    for InstanceSpecGetResponse
{
    fn from(old: v2::instance_spec::InstanceSpecGetResponse) -> Self {
        Self {
            properties: old.properties,
            state: old.state,
            spec: old.spec.into(),
        }
    }
}
