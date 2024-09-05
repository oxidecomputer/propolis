// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helper functions for building instance specs from server parameters.

use propolis_api_types::instance_spec::{v0::*, PciPath};

#[cfg(feature = "falcon")]
use propolis_api_types::instance_spec::components::devices::{
    P9fs, SoftNpuP9, SoftNpuPciPort, SoftNpuPort,
};

mod api_request;
pub(crate) mod builder;
mod config_toml;

/// Describes a storage device/backend pair parsed from an input source like an
/// API request or a config TOML entry.
struct ParsedStorageDevice {
    device_name: String,
    device_spec: StorageDeviceV0,
    backend_name: String,
    backend_spec: StorageBackendV0,
}

/// Describes a network device/backend pair parsed from an input source like an
/// API request or a config TOML entry.
struct ParsedNetworkDevice {
    device_name: String,
    device_spec: NetworkDeviceV0,
    backend_name: String,
    backend_spec: NetworkBackendV0,
}

#[cfg(feature = "falcon")]
#[derive(Default)]
struct ParsedSoftNpu {
    pci_ports: Vec<SoftNpuPciPort>,
    ports: Vec<SoftNpuPort>,
    p9_devices: Vec<SoftNpuP9>,
    p9fs: Vec<P9fs>,
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
