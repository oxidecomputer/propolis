// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! # RFD 605: VM Attestation
//!
//!
//! ## Instance Identity Data
//!
//! Our MVP includes the following identity data for an instance:
//!
//! * boot digest, aka SHA256 hash of the boot disk specified for the instance
//! (iff the instance has a boot disk, and that boot disk is read-only)
//! * instance UUID
//!
//! If there is no boot disk, or the boot disk is not read-only, only the
//! instance ID is used as identifying data.
//!
//! If there is a read-only boot disk, the attestation server will fail
//! challenge requests from guest until the boot disk has been hashed.
//!
//!
//! ## High-Level Design
//!
//! The following assumes that the instance has a vsock device configured.
//! (If there is no vsock device, there will be no attestation server listening
//! there.)
//!
//!  - Guest software submits a 32-byte nonce to a known attestation port.
//!  - This port is backed by a vsock device in propolis.
//!  - When the instance is created (via `instance_ensure`), a tokio task
//!    begins to hash the boot disk of the instance (assuming that a boot disk
//!    is specified and that it is read-only.)
//!  - The attestation server waits on a tokio oneshot channel for the
//!    "VM conf", a structure containing data relevant to instance identity.
//!    This conf is sent to the attestation server once all of the VM identity
//!    data is done (so, in practice, when the boot disk is hashed).
//!  - Until the VM conf is ready, the attestation server fails challenges.
//!  - Once the VM conf is ready, these challenges are passed through to the
//!    sled-agent RoT APIs via the vm_attest crate, and those results are
//!    propagated back to the user.
//!

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub mod boot_digest;
pub mod server;

// See: https://github.com/oxidecomputer/oana
pub const ATTESTATION_PORT: u16 = 605;
pub const ATTESTATION_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), ATTESTATION_PORT);
