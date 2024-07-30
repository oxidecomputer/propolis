// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Backend configuration data: the structs that tell Propolis how to configure
//! its components to talk to other services supplied by the host OS or the
//! larger rack.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A Crucible storage backend.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CrucibleStorageBackend {
    /// A serialized `[crucible_client_types::VolumeConstructionRequest]`. This
    /// is stored in serialized form so that breaking changes to the definition
    /// of a `VolumeConstructionRequest` do not inadvertently break instance
    /// spec deserialization.
    ///
    /// When using a spec to initialize a new instance, the spec author must
    /// ensure this request is well-formed and can be deserialized by the
    /// version of `crucible_client_types` used by the target Propolis.
    pub request_json: String,

    /// Indicates whether the storage is read-only.
    pub readonly: bool,
}

impl std::fmt::Debug for CrucibleStorageBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the contents of the VCR since they may contain volume
        // encryption keys.
        f.debug_struct("CrucibleStorageBackend")
            .field("request_json", &"<redacted>".to_string())
            .field("readonly", &self.readonly)
            .finish()
    }
}

/// A storage backend backed by a file in the host system's file system.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileStorageBackend {
    /// A path to a file that backs a disk.
    pub path: String,

    /// Indicates whether the storage is read-only.
    pub readonly: bool,
}

/// A storage backend for a disk whose initial contents are given explicitly
/// by the specification.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlobStorageBackend {
    /// The disk's initial contents, encoded as a base64 string.
    pub base64: String,

    /// Indicates whether the storage is read-only.
    pub readonly: bool,
}

impl std::fmt::Debug for BlobStorageBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStorageBackend")
            .field("base64", &"<redacted>".to_string())
            .field("readonly", &self.readonly)
            .finish()
    }
}

/// A network backend associated with a virtio-net (viona) VNIC on the host.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtioNetworkBackend {
    /// The name of the viona VNIC to use as a backend.
    pub vnic_name: String,
}

/// A network backend associated with a DLPI VNIC on the host.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DlpiNetworkBackend {
    /// The name of the VNIC to use as a backend.
    pub vnic_name: String,
}
