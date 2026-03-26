// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub mod boot_digest;
pub mod server;

/// TODO: block comment
///
/// - explain vm conf structure: this defines additional data tying the guest challenge (qualifying
/// data) to the instance. Currently it's definition is in the vm_attest crate.
///

// See: https://github.com/oxidecomputer/oana
pub const ATTESTATION_PORT: u16 = 605;
pub const ATTESTATION_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), ATTESTATION_PORT);
