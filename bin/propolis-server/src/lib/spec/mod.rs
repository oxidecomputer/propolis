// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance specs describe how to configure a VM and what components it has.
//!
//! This module defines an "internal" spec type for the server's instance
//! initialization code to use. This spec type is not `Serialize` and is not
//! meant to be sent over the wire in API requests or the migration protocol.
//! This allows the internal representation to change freely between versions
//! (so long as it can consistently be converted to and from the wire-format
//! spec in the [`propolis_api_types`] crate); this, in turn, allows this
//! representation to take forms that might otherwise be hard to change in a
//! backward-compatible way.

use propolis_api_types::instance_spec::{
    components::{
        board::Board,
        devices::{
            PciPciBridge as PciPciBridgeSpec, QemuPvpanic as QemuPvpanicSpec,
        },
    },
    v0::*,
    PciPath,
};

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::{
    backends::DlpiNetworkBackend,
    devices::{P9fs, SoftNpuP9, SoftNpuPciPort},
};

mod api_request;
pub(crate) mod api_spec_v0;
pub(crate) mod builder;
mod config_toml;

#[derive(Clone, Debug, Default)]
pub(crate) struct Spec {
    board: Board,
    disks: Vec<Disk>,
    nics: Vec<Nic>,

    // TODO(#735): Preserve device names for identification purposes.
    serial: [SerialPort; 4],

    // TODO(#735): Preserve device names for identification purposes.
    pci_pci_bridges: Vec<PciPciBridge>,
    pvpanic: Option<QemuPvpanic>,

    #[cfg(feature = "falcon")]
    softnpu: SoftNpu,
}

/// Describes a storage device/backend pair parsed from an input source like an
/// API request or a config TOML entry.
#[derive(Clone, Debug)]
pub struct Disk {
    pub device_name: String,
    pub device_spec: StorageDeviceV0,
    pub backend_name: String,
    pub backend_spec: StorageBackendV0,
}

/// Describes a network device/backend pair parsed from an input source like an
/// API request or a config TOML entry.
#[derive(Clone, Debug)]
pub struct Nic {
    pub device_name: String,
    pub device_spec: NetworkDeviceV0,
    pub backend_name: String,
    pub backend_spec: NetworkBackendV0,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SerialPort {
    #[default]
    Disabled,
    Enabled,

    #[cfg(feature = "falcon")]
    SoftNpu,
}

#[derive(Clone, Debug)]
pub struct QemuPvpanic(pub QemuPvpanicSpec);

#[derive(Clone, Debug)]
pub struct PciPciBridge(pub PciPciBridgeSpec);

impl PciPciBridge {
    pub fn name(&self) -> String {
        format!("pci-bridge-{}", self.0.downstream_bus)
    }
}

#[cfg(feature = "falcon")]
#[derive(Clone, Debug)]
pub struct SoftNpuPort {
    pub name: String,
    pub backend_name: String,
    pub backend_spec: DlpiNetworkBackend,
}

#[cfg(feature = "falcon")]
#[derive(Clone, Debug, Default)]
pub struct SoftNpu {
    pub pci_port: Option<SoftNpuPciPort>,
    pub ports: Vec<SoftNpuPort>,
    pub p9_device: Option<SoftNpuP9>,
    pub p9fs: Option<P9fs>,
}

/// Generates NIC device and backend names from the NIC's PCI path. This is
/// needed because the `name` field in a propolis-client
/// `NetworkInterfaceRequest` is actually the name of the host vNIC to bind to,
/// and that can change between incarnations of an instance. The PCI path is
/// unique to each NIC but must remain stable over a migration, so it's suitable
/// for use in this naming scheme.
///
/// N.B. Migrating a NIC requires the source and target to agree on these names,
///      so changing this routine's behavior will prevent Propolis processes
///      with the old behavior from migrating processes with the new behavior.
fn pci_path_to_nic_names(path: PciPath) -> (String, String) {
    (format!("vnic-{}", path), format!("vnic-{}-backend", path))
}
