//! Backend configuration data: the structs that tell Propolis how to configure
//! its components to talk to other services supplied by the host OS or the
//! larger rack.
//!
//! # Foreign types
//!
//! A full [`BackendSpec`] contains maps from backend names to backend
//! parameters that describe how to set up each backend. This crate assumes that
//! backend parameter versioning is handled by the crate's users: a Propolis
//! client that wants to send a full [`BackendSpec`] is responsible for
//! instantiating a Propolis version that will accept the [`BackendSpec`]
//! definition that the client wants to send (or for setting up a
//! [`BackendSpec`] that its Propolis will understand).
//!
//! During live migration, Propolis makes sure that the source and target have
//! the same backend names and categories, but allows their definitions to
//! differ. The [`BackendNames`] helper type converts from a full
//! [`BackendSpec`] to a collection of backend names that implements an
//! `is_migration_compatible` compatibility check.
//!
//! # Versioning
//!
//! This scheme means that the Propolis live migration procedure does not need
//! to deserialize any backend definitions sent from another Propolis. This
//! means that these definitions can change freely without creating a new
//! version of the `InstanceSpec` (provided that Nexus can still set up both
//! Propolis instances' [`BackendSpec`]s according to the rules above).
//!
//! [`BackendNames`] are, by contrast, a load-bearing part of the migration
//! protocol and need some care to be versioned correctly. See the struct
//! comments below for more information.

use std::collections::{BTreeMap, BTreeSet};

use super::{MigrationCollection, MigrationCompatibilityError, SpecKey};
use serde::{Deserialize, Serialize};

/// A kind of storage backend: a connection to on-sled resources or other
/// services that provide the functions storage devices need to implement their
/// contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum StorageBackendKind {
    /// A Crucible-backed device, containing a construction request.
    Crucible { req: crucible_client_types::VolumeConstructionRequest },

    /// A device backed by a file on the host machine. The payload is a path to
    /// this file.
    File { path: String },

    /// A device backed by an in-memory buffer in the VMM process. The initial
    /// contents of the disk are a base64-encoded string.
    InMemory { base64: String },
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

/// A kind of network backend: a connection to an on-sled networking resource
/// that provides the functions needed for guest network adapters to implement
/// their contracts.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub enum NetworkBackendKind {
    /// A virtio-net (viona) backend associated with the supplied named vNIC on
    /// the host.
    Virtio { vnic_name: String },

    /// A DLPI backend associated with the supplied named vNIC on the host.
    Dlpi { vnic_name: String },
}

/// A network backend.
#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct NetworkBackend {
    pub kind: NetworkBackendKind,
}

/// A wrapper type for all the backends in an instance spec.
#[derive(Default, Clone, Deserialize, Serialize, Debug)]
pub struct BackendSpec {
    pub storage_backends: BTreeMap<SpecKey, StorageBackend>,
    pub network_backends: BTreeMap<SpecKey, NetworkBackend>,
}

/// A helper type representing just the names of the backends in a particular
/// [`BackendSpec`].
//
// This struct is marked `deny_unknown_fields` so that if a new backend type is
// added in version N, version N-1 will refuse to deserialize it (desirable
// since version N-1 presumably does not know how to instantiate such a
// backend).
//
// If version N's new collection is empty, version N-1 has no backends to
// instantiate, so the field can be completely omitted using
// `skip_serializing_if`. This in turn requires the struct to be marked with
// `serde(default)` so that serde can deserialize structs with omitted
// name sets.
#[derive(Default, Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BackendNames {
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    storage: BTreeSet<SpecKey>,

    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    network: BTreeSet<SpecKey>,
}

impl From<&BackendSpec> for BackendNames {
    fn from(spec: &BackendSpec) -> Self {
        Self {
            storage: spec.storage_backends.keys().cloned().collect(),
            network: spec.network_backends.keys().cloned().collect(),
        }
    }
}

impl BackendNames {
    /// Indicates whether two backend specs with the supplied named backends are
    /// migration-compatible with each other.
    pub fn can_migrate_backends_from(
        &self,
        other: &Self,
    ) -> Result<(), MigrationCompatibilityError> {
        self.storage.can_migrate_from_collection(&other.storage).map_err(
            |e| {
                MigrationCompatibilityError::CollectionMismatch(
                    "storage backends".to_string(),
                    e,
                )
            },
        )?;

        self.network.can_migrate_from_collection(&other.network).map_err(
            |e| {
                MigrationCompatibilityError::CollectionMismatch(
                    "network backends".to_string(),
                    e,
                )
            },
        )?;

        Ok(())
    }
}
