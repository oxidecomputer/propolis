//! Backend configuration data: the structs that tell Propolis how to configure
//! its components to talk to other services supplied by the host OS or the
//! larger rack.

use std::collections::BTreeMap;
use std::convert::TryFrom;

use super::{MigrationCompatible, SpecKey, SpecMismatchDetails};
use serde::{Deserialize, Serialize};

/// A description of a Crucible volume construction request.
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct CrucibleRequestContents {
    /// A [`crucible_client_types::VolumeConstructionRequest`], serialized as JSON.
    //
    // Storing volume construction requests in serialized form allows external
    // types to change without causing a breaking change to instance specs.
    // Consider the following scenario, assuming the VolumeConstructionRequest
    // struct is used directly:
    //
    // - Sled agent v1 starts Propolis v1 using Crucible request v1.
    // - Sled agent v2 (on some other sled with newer software) starts Propolis
    //   v2 using Crucible request v2, which has a new field not present in v1.
    // - Nexus orders a migration from v1 to v2. This requires someone to
    //   compare the two instances' specs for migratability.
    //
    // Migration compatibility is normally checked by the two Propolis servers
    // involved: one server sends its instance spec to the other, and the
    // recipient compares the specs to see if they're compatible. In this case,
    // v2 can't deserialize v1's spec (a field is missing), and v1 can't
    // deserialize v2's (an extra field is present), so migration will always
    // fail.
    //
    // Storing a serialized request avoids this problem as follows:
    //
    // - Sled agent v2 starts Propolis v2 with spec v2. It deserializes the
    //   request contents in the spec body into a v2 construction request.
    // - Migration begins. Propolis v2 can now deserialize the v1 instance spec
    //   and check it for compatibility. It can't deserialize the v1 *request
    //   contents*, but this can be dealt with separately (e.g. by having the v1
    //   and v2 Crucible components in the Propolis server negotiate
    //   compatibility themselves, which is an affordance the migration protocol
    //   allows).
    pub json: String,
}

impl TryFrom<&CrucibleRequestContents>
    for crucible_client_types::VolumeConstructionRequest
{
    type Error = serde_json::Error;

    fn try_from(value: &CrucibleRequestContents) -> Result<Self, Self::Error> {
        serde_json::from_str(&value.json)
    }
}

impl TryFrom<&crucible_client_types::VolumeConstructionRequest>
    for CrucibleRequestContents
{
    type Error = serde_json::Error;

    fn try_from(
        value: &crucible_client_types::VolumeConstructionRequest,
    ) -> Result<Self, Self::Error> {
        Ok(Self { json: serde_json::to_string(value)? })
    }
}

/// A kind of storage backend: a connection to on-sled resources or other
/// services that provide the functions storage devices need to implement their
/// contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum StorageBackendKind {
    /// A Crucible-backed device, containing a generation number and a
    /// construction request.
    Crucible { gen: u64, req: CrucibleRequestContents },

    /// A device backed by a file on the host machine. The payload is a path to
    /// this file.
    File { path: String },

    /// A device backed by an in-memory buffer in the VMM process. The initial
    /// contents of this buffer are supplied out-of-band, either at
    /// instance initialization time or from a migration source.
    InMemory,
}

impl MigrationCompatible for StorageBackendKind {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            Err(SpecMismatchDetails::StorageBackendKind(
                self.clone(),
                other.clone(),
            ))
        } else {
            Ok(())
        }
    }
}

/// A storage backend.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct StorageBackend {
    /// The kind of storage backend this is.
    pub kind: StorageBackendKind,

    /// Whether the storage is read-only.
    pub readonly: bool,
}

impl MigrationCompatible for StorageBackend {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        self.kind.is_migration_compatible(&other.kind)?;
        if self.readonly != other.readonly {
            Err(SpecMismatchDetails::StorageBackendReadonly(
                self.readonly,
                other.readonly,
            ))
        } else {
            Ok(())
        }
    }
}

/// A kind of network backend: a connection to an on-sled networking resource
/// that provides the functions needed for guest network adapters to implement
/// their contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum NetworkBackendKind {
    /// A virtio-net (viona) backend associated with the supplied named vNIC on
    /// the host.
    Virtio { vnic_name: String },
}

impl MigrationCompatible for NetworkBackendKind {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        // Two network backends are compatible if they have the same kind,
        // irrespective of their configurations.
        if std::mem::discriminant(self) != std::mem::discriminant(other) {
            return Err(SpecMismatchDetails::NetworkBackendKind(
                self.clone(),
                other.clone(),
            ));
        }

        Ok(())
    }
}

/// A network backend.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct NetworkBackend {
    pub kind: NetworkBackendKind,
}

impl MigrationCompatible for NetworkBackend {
    fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), SpecMismatchDetails> {
        self.kind.is_migration_compatible(&other.kind)
    }
}

#[derive(Default, Clone, Deserialize, Serialize, Debug)]
pub struct BackendSpec {
    pub storage_backends: BTreeMap<SpecKey, StorageBackend>,
    pub network_backends: BTreeMap<SpecKey, NetworkBackend>,
}

impl BackendSpec {
    pub fn is_migration_compatible(
        &self,
        other: &Self,
    ) -> Result<(), (String, SpecMismatchDetails)> {
        self.storage_backends
            .is_migration_compatible(&other.storage_backends)
            .map_err(|e| ("storage backends".to_string(), e))?;

        self.network_backends
            .is_migration_compatible(&other.network_backends)
            .map_err(|e| ("network backends".to_string(), e))?;

        Ok(())
    }
}
