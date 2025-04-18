// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Abstractions for disks with an in-memory backend.

use propolis_client::instance_spec::{BlobStorageBackend, ComponentV0};

use super::DiskConfig;
use crate::disk::DeviceName;

/// A disk with an in-memory backend.
#[derive(Debug)]
pub struct InMemoryDisk {
    device_name: DeviceName,
    contents: Vec<u8>,
    readonly: bool,
}

impl InMemoryDisk {
    /// Creates an in-memory backend that will present the supplied `contents`
    /// to the guest.
    pub fn new(
        device_name: DeviceName,
        contents: Vec<u8>,
        readonly: bool,
    ) -> Self {
        Self { device_name, contents, readonly }
    }
}

impl DiskConfig for InMemoryDisk {
    fn device_name(&self) -> &DeviceName {
        &self.device_name
    }

    fn backend_spec(&self) -> ComponentV0 {
        let base64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            self.contents.as_slice(),
        );

        ComponentV0::BlobStorageBackend(BlobStorageBackend {
            base64,
            readonly: self.readonly,
        })
    }

    fn guest_os(&self) -> Option<crate::guest_os::GuestOsKind> {
        None
    }
}
