// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Definitions for types exposed by the propolis-server API

use std::{collections::HashMap, fmt, net::SocketAddr};

use instance_spec::{components::backends, v0::InstanceSpecV0, SpecKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Re-export the instance spec boot settings types so they can also be used in
// legacy instance ensure requests.
pub use crate::instance_spec::components::devices::{
    BootOrderEntry, BootSettings,
};
use crate::instance_spec::VersionedInstanceSpec;

// Re-export volume construction requests since they're part of a disk request.
pub use crucible_client_types::VolumeConstructionRequest;

pub mod instance_spec;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceVCRReplace {
    pub vcr_json: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceNameParams {
    pub instance_id: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstancePathParams {
    pub instance_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct InstanceMetadata {
    pub silo_id: Uuid,
    pub project_id: Uuid,
    pub sled_id: Uuid,
    pub sled_serial: String,
    pub sled_revision: u32,
    pub sled_model: String,
}

#[derive(Clone, Deserialize, Serialize, Debug, JsonSchema)]
#[serde(deny_unknown_fields, tag = "type", content = "component")]
pub enum ReplacementComponent {
    CrucibleStorageBackend(backends::CrucibleStorageBackend),
    FileStorageBackend(backends::FileStorageBackend),
    BlobStorageBackend(backends::BlobStorageBackend),
    VirtioNetworkBackend(backends::VirtioNetworkBackend),
    DlpiNetworkBackend(backends::DlpiNetworkBackend),
}

impl From<ReplacementComponent> for instance_spec::v0::ComponentV0 {
    fn from(value: ReplacementComponent) -> Self {
        use instance_spec::v0::ComponentV0;
        match value {
            ReplacementComponent::CrucibleStorageBackend(be) => {
                ComponentV0::CrucibleStorageBackend(be)
            }
            ReplacementComponent::FileStorageBackend(be) => {
                ComponentV0::FileStorageBackend(be)
            }
            ReplacementComponent::BlobStorageBackend(be) => {
                ComponentV0::BlobStorageBackend(be)
            }
            ReplacementComponent::VirtioNetworkBackend(be) => {
                ComponentV0::VirtioNetworkBackend(be)
            }
            ReplacementComponent::DlpiNetworkBackend(be) => {
                ComponentV0::DlpiNetworkBackend(be)
            }
        }
    }
}

/// The mechanism to use to create a new Propolis VM.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "method", content = "value")]
pub enum InstanceInitializationMethod {
    /// Create a brand new VM with the devices specified in the configuration
    /// TOML passed to propolis-server when it started.
    ConfigToml { cpus: u8, memory_mib: u64 },

    /// Create a brand new VM with the devies specified in the supplied spec.
    Spec {
        /// The component manifest for the new VM.
        spec: InstanceSpecV0,
    },

    /// Initialize the VM via migration.
    MigrationTarget {
        /// The ID to assign to this migration attempt.
        migration_id: Uuid,

        /// The address of the Propolis server that will serve as the migration
        /// source.
        src_addr: SocketAddr,

        /// A list of components in the source VM's instance spec to replace.
        replace_components: HashMap<SpecKey, ReplacementComponent>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureRequest {
    pub properties: InstanceProperties,
    pub init: InstanceInitializationMethod,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureResponse {
    pub migrate: Option<InstanceMigrateInitiateResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceMigrateInitiateResponse {
    pub migration_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceMigrateStartRequest {
    pub migration_id: Uuid,
}

/// The status of an individual live migration.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct InstanceMigrationStatus {
    /// The ID of this migration, supplied either by the external migration
    /// requester (for targets) or the other side of the migration (for
    /// sources).
    pub id: Uuid,
    /// The current phase the migration is in.
    pub state: MigrationState,
}

/// The statuses of the most recent attempts to live migrate into and out of
/// this Propolis.
///
/// If a VM is initialized by migration in and then begins to migrate out, this
/// structure will contain statuses for both migrations. This ensures that
/// clients can always obtain the status of a successful migration in even after
/// a migration out begins.
///
/// This structure only reports the status of the most recent migration in a
/// single direction. That is, if a migration in or out fails, and a new
/// migration attempt begins, the new migration's status replaces the old's.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct InstanceMigrateStatusResponse {
    /// The status of the most recent attempt to initialize the current instance
    /// via migration in, or `None` if the instance has never been a migration
    /// target.
    pub migration_in: Option<InstanceMigrationStatus>,
    /// The status of the most recent attempt to migrate out of the current
    /// instance, or `None` if the instance has never been a migration source.
    pub migration_out: Option<InstanceMigrationStatus>,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    JsonSchema,
)]
pub enum MigrationState {
    Sync,
    RamPush,
    Pause,
    RamPushDirty,
    Device,
    Resume,
    RamPull,
    Server,
    Finish,
    Error,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceGetResponse {
    pub instance: Instance,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecGetResponse {
    pub properties: InstanceProperties,
    pub state: InstanceState,

    /// The instance's component manifest, if it is known at this point.
    /// (Instances that initialize via live migration receive specs from their
    /// sources, so this field will be None for an instance that is still
    /// initializing via migration.)
    pub spec: Option<VersionedInstanceSpec>,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorRequest {
    pub gen: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorResponse {
    pub gen: u64,
    pub state: InstanceState,
    pub migration: InstanceMigrateStatusResponse,
}

/// Requested state of an Instance.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateChange {
    pub state: InstanceStateRequested,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum InstanceStateRequested {
    Run,
    Stop,
    Reboot,
}

/// Current state of an Instance.
#[derive(
    Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema,
)]
pub enum InstanceState {
    Creating,
    Starting,
    Running,
    Stopping,
    Stopped,
    Rebooting,
    Migrating,
    Repairing,
    Failed,
    Destroyed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
pub struct InstanceProperties {
    /// Unique identifier for this Instance.
    pub id: Uuid,
    /// Human-readable name of the Instance.
    pub name: String,
    /// Free-form text description of an Instance.
    pub description: String,
    /// Metadata used to track statistics for this Instance.
    pub metadata: InstanceMetadata,
}

impl InstanceProperties {
    /// Return the name of the VMM resource backing this VM.
    pub fn vm_name(&self) -> String {
        self.id.to_string()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Instance {
    pub properties: InstanceProperties,
    pub state: InstanceState,
}

/// Request a specific range of an Instance's serial console output history.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct InstanceSerialConsoleHistoryRequest {
    /// Character index in the serial buffer from which to read, counting the bytes output since
    /// instance start. If this is not provided, `most_recent` must be provided, and if this *is*
    /// provided, `most_recent` must *not* be provided.
    pub from_start: Option<u64>,
    /// Character index in the serial buffer from which to read, counting *backward* from the most
    /// recently buffered data retrieved from the instance. (See note on `from_start` about mutual
    /// exclusivity)
    pub most_recent: Option<u64>,
    /// Maximum number of bytes of buffered serial console contents to return. If the requested
    /// range runs to the end of the available buffer, the data returned will be shorter than
    /// `max_bytes`.
    pub max_bytes: Option<u64>,
}

/// Contents of an Instance's serial console buffer.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSerialConsoleHistoryResponse {
    /// The bytes starting from the requested offset up to either the end of the buffer or the
    /// request's `max_bytes`. Provided as a u8 array rather than a string, as it may not be UTF-8.
    pub data: Vec<u8>,
    /// The absolute offset since boot (suitable for use as `byte_offset` in a subsequent request)
    /// of the last byte returned in `data`.
    pub last_byte_offset: u64,
}

/// Connect to an Instance's serial console via websocket, optionally sending
/// bytes from the buffered history first.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct InstanceSerialConsoleStreamRequest {
    /// Character index in the serial buffer from which to read, counting the bytes output since
    /// instance start. If this is provided, `most_recent` must *not* be provided.
    // TODO: if neither is specified, send enough serial buffer history to reconstruct
    //  the current contents and cursor state of an interactive terminal
    pub from_start: Option<u64>,
    /// Character index in the serial buffer from which to read, counting *backward* from the most
    /// recently buffered data retrieved from the instance. (See note on `from_start` about mutual
    /// exclusivity)
    pub most_recent: Option<u64>,
}

/// Control message(s) sent through the websocket to serial console clients.
///
/// Note: Because this is associated with the websocket, and not some REST
/// endpoint, Dropshot lacks the ability to communicate it via the OpenAPI
/// document underpinning the exposed interfaces.  As such, clients (including
/// the `propolis-client` crate) are expected to define their own identical copy
/// of this type in order to consume it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InstanceSerialConsoleControlMessage {
    Migrating { destination: SocketAddr, from_start: u64 },
}

#[derive(Deserialize, JsonSchema)]
pub struct SnapshotRequestPathParams {
    pub id: SpecKey,
    pub snapshot_id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
pub struct VCRRequestPathParams {
    pub id: SpecKey,
}

#[derive(Deserialize, JsonSchema)]
pub struct VolumeStatusPathParams {
    pub id: SpecKey,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct VolumeStatus {
    pub active: bool,
}

/// Error codes used to populate the `error_code` field of Dropshot API responses.
#[derive(
    Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema,
)]
pub enum ErrorCode {
    /// This `propolis-server` process has not received an `InstanceEnsure`
    /// request yet.
    NoInstance,
    /// This `propolis-server` process has already received an `InstanceEnsure`
    /// request with a different ID.
    AlreadyInitialized,
    /// Cannot update a running server.
    AlreadyRunning,
    /// Instance creation failed
    CreateFailed,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::str::FromStr for ErrorCode {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            s if s.eq_ignore_ascii_case("NoInstance") => Ok(Self::NoInstance),
            s if s.eq_ignore_ascii_case("AlreadyInitialized") => {
                Ok(ErrorCode::AlreadyInitialized)
            }
            s if s.eq_ignore_ascii_case("AlreadyRunning") => {
                Ok(ErrorCode::AlreadyRunning)
            }
            s if s.eq_ignore_ascii_case("CreateFailed") => {
                Ok(ErrorCode::CreateFailed)
            }
            _ => Err("unknown error code, expected one of: \
                'NoInstance', 'AlreadyInitialized', 'AlreadyRunning', \
                'CreateFailed'"),
        }
    }
}
