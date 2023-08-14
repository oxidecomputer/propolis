// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Types used to communicate between the Sled Agent and Propolis.
//!
//! Although many of these types mirror the API between Nexus
//! and sled agent (within omicron-common), they are intentionally
//! decoupled so the interfaces may evolve independently, as necessary.

use crate::instance_spec::InstanceSpec;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use uuid::Uuid;

// Re-export types that are of a public struct
pub use crucible_client_types::VolumeConstructionRequest;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceNameParams {
    pub instance_id: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstancePathParams {
    pub instance_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureRequest {
    pub properties: InstanceProperties,

    #[serde(default)]
    pub nics: Vec<NetworkInterfaceRequest>,

    #[serde(default)]
    pub disks: Vec<DiskRequest>,

    pub migrate: Option<InstanceMigrateInitiateRequest>,

    // base64 encoded cloud-init ISO
    pub cloud_init_bytes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceSpecEnsureRequest {
    pub properties: InstanceProperties,
    pub instance_spec: InstanceSpec,
    pub migrate: Option<InstanceMigrateInitiateRequest>,
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceMigrateStatusRequest {
    pub migration_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct InstanceMigrateStatusResponse {
    pub migration_id: Uuid,
    pub state: MigrationState,
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
    pub spec: InstanceSpec,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorRequest {
    pub gen: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorResponse {
    pub gen: u64,
    pub state: InstanceState,
    pub migration: Option<InstanceMigrateStatusResponse>,
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

    /// ID of the image used to initialize this Instance.
    pub image_id: Uuid,
    /// ID of the bootrom used to initialize this Instance.
    pub bootrom_id: Uuid,
    /// Size of memory allocated to the Instance, in MiB.
    pub memory: u64,
    /// Number of vCPUs to be allocated to the Instance.
    pub vcpus: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Instance {
    pub properties: InstanceProperties,
    pub state: InstanceState,

    pub disks: Vec<DiskAttachment>,
    pub nics: Vec<NetworkInterface>,
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

/// Send things besides raw bytes to connected serial console clients, such as
/// the change-of-address notice when the VM migrates
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InstanceSerialConsoleControlMessage {
    Migrating { destination: SocketAddr, from_start: u64 },
}

/// Describes how to connect to one or more storage agent services.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct StorageAgentDescription {
    /// Addresses of storage agents.
    pub agents: Vec<std::net::SocketAddrV6>,

    /// Opaque key material for encryption and decryption.
    /// May become more structured as encryption scheme is solidified.
    pub key: Vec<u8>,

    /// Minimum number of redundant copies of a block which must
    /// be written until data is considered "persistent".
    pub write_redundancy_threshold: u32,
}

/// Refer to RFD 135 for more information on Virtual Storage Interfaces.
/// This describes the type of disk which should be exposed to the guest VM.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
pub enum DiskType {
    NVMe,
    VirtioBlock,
}

/// Describes a virtual disk.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Disk {
    /// Unique identifier for this disk.
    pub id: Uuid,

    /// Storage agents which implement networked block device servers.
    pub storage_agents: StorageAgentDescription,

    /// Size of the disk (blocks).
    pub block_count: u64,

    /// Block size (bytes).
    pub block_size: u32,

    /// Storage interface.
    pub interface: DiskType,
}

// TODO: Struggling to make this struct derive "JsonSchema"
/*
bitflags! {
    #[derive(Deserialize, Serialize)]
    pub struct DiskFlags: u32 {
        const READ = 0b0000_0001;
        const WRITE = 0b0000_0010;
        const READ_WRITE = Self::READ.bits | Self::WRITE.bits;
    }
}
*/

// TODO: Remove this; it's a hack to workaround the above bug.
#[allow(dead_code)]
pub const DISK_FLAG_READ: u32 = 0b0000_0001;
#[allow(dead_code)]
pub const DISK_FLAG_WRITE: u32 = 0b0000_0010;
#[allow(dead_code)]
pub const DISK_FLAG_READ_WRITE: u32 = DISK_FLAG_READ | DISK_FLAG_WRITE;
type DiskFlags = u32;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct DiskRequest {
    pub name: String,
    pub slot: Slot,
    pub read_only: bool,
    pub device: String,

    // Crucible related opts
    pub volume_construction_request:
        crucible_client_types::VolumeConstructionRequest,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct DiskAttachmentInfo {
    pub flags: DiskFlags,
    pub slot: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub enum DiskAttachmentState {
    Attached(Uuid),
    Detached,
    Destroyed,
    Faulted,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct DiskAttachment {
    pub generation_id: u64,
    pub disk_id: Uuid,
    pub state: DiskAttachmentState,
}

/// A stable index which is translated by Propolis
/// into a PCI BDF, visible to the guest.
#[derive(Copy, Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Slot(pub u8);

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct NetworkInterfaceRequest {
    pub name: String,
    pub slot: Slot,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct NetworkInterface {
    pub name: String,
    pub attachment: NetworkInterfaceAttachmentState,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub enum NetworkInterfaceAttachmentState {
    Attached(Slot),
    Detached,
    Faulted,
}

#[derive(Deserialize, JsonSchema)]
pub struct SnapshotRequestPathParams {
    pub id: Uuid,
    pub snapshot_id: Uuid,
}
