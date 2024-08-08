// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Abstractions for disks with an in-memory backend.

use propolis_client::types::{BlobStorageBackend, StorageBackendV0};

use super::DiskConfig;

/// A disk with an in-memory backend.
#[derive(Debug)]
pub struct InMemoryDisk {
    backend_name: String,
    contents: Vec<u8>,
    readonly: bool,
}

impl InMemoryDisk {
    /// Creates an in-memory backend that will present the supplied `contents`
    /// to the guest.
    pub fn new(
        backend_name: String,
        contents: Vec<u8>,
        readonly: bool,
    ) -> Self {
        Self { backend_name, contents, readonly }
    }
}

impl DiskConfig for InMemoryDisk {
    fn backend_spec(&self) -> (String, StorageBackendV0) {
        let base64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            self.contents.as_slice(),
        );

        (
            self.backend_name.clone(),
            StorageBackendV0::Blob(BlobStorageBackend {
                base64,
                readonly: self.readonly,
            }),
        )
    }

    fn guest_os(&self) -> Option<crate::guest_os::GuestOsKind> {
        None
    }
}
