// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use crate::instance_spec::{components, SpecKey, VersionedInstanceSpec};
use crate::{InstanceProperties, InstanceState};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "component",
    rename_all = "snake_case"
)]
pub enum ComponentV0 {
    VirtioDisk(components::devices::VirtioDisk),
    NvmeDisk(components::devices::NvmeDisk),
    VirtioNic(components::devices::VirtioNic),
    SerialPort(components::devices::SerialPort),
    PciPciBridge(components::devices::PciPciBridge),
    QemuPvpanic(components::devices::QemuPvpanic),
    BootSettings(components::devices::BootSettings),
    SoftNpuPciPort(components::devices::SoftNpuPciPort),
    SoftNpuPort(components::devices::SoftNpuPort),
    SoftNpuP9(components::devices::SoftNpuP9),
    P9fs(components::devices::P9fs),
    MigrationFailureInjector(components::devices::MigrationFailureInjector),
    CrucibleStorageBackend(components::backends::CrucibleStorageBackend),
    FileStorageBackend(components::backends::FileStorageBackend),
    BlobStorageBackend(components::backends::BlobStorageBackend),
    VirtioNetworkBackend(components::backends::VirtioNetworkBackend),
    DlpiNetworkBackend(components::backends::DlpiNetworkBackend),
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceSpecV0 {
    pub board: components::board::Board,
    pub components: BTreeMap<SpecKey, ComponentV0>,
}

impl InstanceSpecV0 {
    /// Convert this [`InstanceSpecV0`] into an [`InstanceSpec`], the following
    /// version of this type as used in the API.
    ///
    /// This is not a `TryFrom`, `InstanceSpec` includes a new
    /// `SmbiosType1Input` field which moves the UUID included in that table
    /// into the instance spec. Previously, this had been ambiently provided by
    /// `propolis-server`, which set it to the same ID as provided by
    /// `InstanceProperties`.
    ///
    /// In typical use of `propolis-server` (deployed in a rack, operated by
    /// sled-agent and Nexus, etc), this UUID will be set the same as
    /// `propolis-server` had bee defaulting. But other use of `propolis-server`
    /// (falcon, a4x2) may vary it for testing and emulating non-rack
    /// VM configurations.
    pub fn into_instance_spec(self, smbios_uuid: &str) -> crate::InstanceSpec {
        let Self { board, components } = self;

        crate::InstanceSpec {
            board,
            components,
            smbios: crate::SmbiosType1Input::ox_vm_v0(smbios_uuid),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecGetResponseV0 {
    pub properties: InstanceProperties,
    pub state: InstanceState,
    pub spec: InstanceSpecStatusV0,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value")]
pub enum InstanceSpecStatusV0 {
    WaitingForMigrationSource,
    Present(VersionedInstanceSpec),
}
