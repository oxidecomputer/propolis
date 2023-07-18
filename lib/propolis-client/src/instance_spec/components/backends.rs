//! Backend configuration data: the structs that tell Propolis how to configure
//! its components to talk to other services supplied by the host OS or the
//! larger rack.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A Crucible storage backend.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
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

/// A storage backend backed by a file in the host system's file system.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileStorageBackend {
    /// A path to a file that backs a disk.
    pub path: String,

    /// Indicates whether the storage is read-only.
    pub readonly: bool,
}

/// A storage backend backed by an in-memory buffer in the Propolis process.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InMemoryStorageBackend {
    /// The initial contents of the in-memory disk.
    pub base64: String,

    /// Indicates whether the storage is read-only.
    pub readonly: bool,
}

/// A network backend associated with a virtio-net (viona) VNIC on the host.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtioNetworkBackend {
    /// The name of the viona VNIC to use as a backend.
    vnic_name: String,
}

/// A network backend associated with a DLPI VNIC on the host.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DlpiNetworkBackend {
    /// The name of the VNIC to use as a backend.
    vnic_name: String,
}
