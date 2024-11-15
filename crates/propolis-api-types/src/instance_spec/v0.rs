// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;

use crate::instance_spec::{components, SpecKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", content = "component")]
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
