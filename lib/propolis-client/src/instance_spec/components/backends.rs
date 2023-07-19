//! Backend configuration data: the structs that tell Propolis how to configure
//! its components to talk to other services supplied by the host OS or the
//! larger rack.

use crate::instance_spec::migration::MigrationElement;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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

impl MigrationElement for CrucibleStorageBackend {
    fn kind(&self) -> &'static str {
        "CrucibleStorageBackend"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.readonly != other.readonly {
            Err(MigrationCompatibilityError::ComponentConfiguration(format!(
                "read-only mismatch (self: {}, other: {})",
                self.readonly, other.readonly,
            ))
            .into())
        } else {
            Ok(())
        }
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

impl MigrationElement for FileStorageBackend {
    fn kind(&self) -> &'static str {
        "FileStorageBackend"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.readonly != other.readonly {
            Err(MigrationCompatibilityError::ComponentConfiguration(format!(
                "read-only mismatch (self: {}, other: {})",
                self.readonly, other.readonly,
            ))
            .into())
        } else {
            Ok(())
        }
    }
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

impl MigrationElement for InMemoryStorageBackend {
    fn kind(&self) -> &'static str {
        "InMemoryStorageBackend"
    }

    fn can_migrate_from_element(
        &self,
        other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        if self.readonly != other.readonly {
            Err(MigrationCompatibilityError::ComponentConfiguration(format!(
                "read-only mismatch (self: {}, other: {})",
                self.readonly, other.readonly,
            ))
            .into())
        } else {
            Ok(())
        }
    }
}

/// A network backend associated with a virtio-net (viona) VNIC on the host.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtioNetworkBackend {
    /// The name of the viona VNIC to use as a backend.
    pub vnic_name: String,
}

impl MigrationElement for VirtioNetworkBackend {
    fn kind(&self) -> &'static str {
        "VirtioNetworkBackend"
    }

    fn can_migrate_from_element(
        &self,
        _other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        Ok(())
    }
}

/// A network backend associated with a DLPI VNIC on the host.
#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DlpiNetworkBackend {
    /// The name of the VNIC to use as a backend.
    pub vnic_name: String,
}

impl MigrationElement for DlpiNetworkBackend {
    fn kind(&self) -> &'static str {
        "DlpiNetworkBackend"
    }

    fn can_migrate_from_element(
        &self,
        _other: &Self,
    ) -> Result<(), crate::instance_spec::migration::ElementCompatibilityError>
    {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum MigrationCompatibilityError {
    #[error("component configurations incompatible: {0}")]
    ComponentConfiguration(String),
}
