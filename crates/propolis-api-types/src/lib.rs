// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Definitions for types exposed by the propolis-server API

use std::{collections::BTreeMap, fmt, net::SocketAddr};

use instance_spec::v0::InstanceSpecV0;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Re-export the instance spec boot settings types so they can also be used in
// legacy instance ensure requests.
pub use crate::instance_spec::components::devices::{
    BootOrderEntry, BootSettings,
};
use crate::instance_spec::{
    components, v0::ComponentV0, VersionedInstanceSpec,
};

// Re-export volume construction requests since they're part of a disk request.
pub use crucible_client_types::VolumeConstructionRequest;

pub mod instance_spec;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceVCRReplace {
    pub name: String,
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

/// An instance spec component that should be replaced during a live migration.
///
/// When a caller asks Propolis to initialize via live migration, the target VM
/// inherits the migration source's current instance spec. For the most part,
/// the target can (and indeed in some cases must) use this spec without
/// modifying it; this helps Propolis ensure that guest-visible configuration
/// remains unchanged when a VM migrates. However, there are some components
/// with no guest-visible state that may need to be reconfigured when a VM
/// migrates. These include the following:
///
/// - Crucible disks: After migrating, the target Propolis presents itself as a
///   new client of the Crucible downstairs servers backing the VM's disks.
///   Crucible requires the target to present a newer client generation number
///   to allow the target to connect. In a full Oxide deployment, these numbers
///   are managed by the control plane (i.e. it is not safe for Propolis to
///   manage these values directly--new Crucible volume connection information
///   must always come from Nexus).
/// - Virtio network devices: Each virtio NIC in the guest needs to bind to a
///   named VNIC object on the host. These names can change when a VM migrates
///   from host to host.
///
/// Each component that can be reconfigured this way has a variant in this enum;
/// components not in the enum can't be reconfigured during migration. This
/// saves the initialization API from having to reason about requests to replace
/// a component that can't legally be replaced.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "component", content = "spec")]
pub enum ReplacementComponent {
    MigrationFailureInjector(components::devices::MigrationFailureInjector),
    CrucibleStorageBackend(components::backends::CrucibleStorageBackend),
    VirtioNetworkBackend(components::backends::VirtioNetworkBackend),
}

impl From<ReplacementComponent> for instance_spec::v0::ComponentV0 {
    fn from(value: ReplacementComponent) -> Self {
        match value {
            ReplacementComponent::MigrationFailureInjector(c) => {
                ComponentV0::MigrationFailureInjector(c)
            }
            ReplacementComponent::CrucibleStorageBackend(c) => {
                ComponentV0::CrucibleStorageBackend(c)
            }
            ReplacementComponent::VirtioNetworkBackend(c) => {
                ComponentV0::VirtioNetworkBackend(c)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "method", content = "value")]
pub enum InstanceInitializationMethod {
    Spec {
        spec: InstanceSpecV0,
    },
    MigrationTarget {
        migration_id: Uuid,
        src_addr: SocketAddr,
        replace_components: BTreeMap<String, ReplacementComponent>,
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
pub struct InstanceMigrateInitiateRequest {
    pub migration_id: Uuid,
    pub src_addr: SocketAddr,
    pub src_uuid: Uuid,
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
#[serde(tag = "type", content = "value")]
pub enum InstanceSpecStatus {
    WaitingForMigrationSource,
    Present(VersionedInstanceSpec),
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecGetResponse {
    pub properties: InstanceProperties,
    pub state: InstanceState,
    pub spec: InstanceSpecStatus,
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
    pub id: Uuid,
    pub snapshot_id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
pub struct VCRRequestPathParams {
    pub id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
pub struct VolumeStatusPathParams {
    pub id: Uuid,
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
