//! Types used to communicate between the Sled Agent and Propolis.
//!
//! Although many of these types mirror the API between Nexus
//! and sled agent (within omicron-common), they are intentionally
//! decoupled so the interfaces may evolve independently, as necessary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstancePathParams {
    pub instance_id: Uuid,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureRequest {
    pub properties: InstanceProperties,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstanceEnsureResponse {}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstanceGetResponse {
    pub instance: Instance,
}

/// Requested state of an Instance.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateChange {
    pub state: InstanceStateRequested,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub enum InstanceStateRequested {
    Run,
    Stop,
    Reboot,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstanceSerialRequest {
    pub bytes: Vec<u8>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct InstanceSerialResponse {
    pub bytes: Vec<u8>,
}

/// Current state of an Instance.
#[derive(Deserialize, Serialize, JsonSchema)]
pub enum InstanceState {
    Creating,
    Starting,
    Running,
    Stopping,
    Stopped,
    Repairing,
    Failed,
    Destroyed,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceProperties {
    pub generation_id: u64,

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

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Instance {
    pub properties: InstanceProperties,
    pub state: InstanceState,

    pub disks: Vec<DiskAttachment>,
    pub nics: Vec<NicAttachment>,
}

/// Describes how to connect to one or more storage agent services.
#[derive(Deserialize, Serialize, JsonSchema)]
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
#[derive(Deserialize, Serialize, JsonSchema)]
pub enum DiskType {
    NVMe,
    VirtioBlock,
}

/// Describes a virtual disk.
#[derive(Deserialize, Serialize, JsonSchema)]
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

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DiskAttachmentInfo {
    pub flags: DiskFlags,
    pub slot: u16,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub enum DiskAttachmentState {
    Attached(Uuid),
    Detached,
    Destroyed,
    Faulted,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DiskAttachment {
    pub generation_id: u64,
    pub disk_id: Uuid,
    pub state: DiskAttachmentState,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct NicAttachmentInfo {
    pub slot: u16,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub enum NicAttachmentState {
    Attached(NicAttachmentInfo),
    Detached,
    Faulted,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct NicAttachment {
    pub generation_id: u64,
    pub nic_id: Uuid,
    pub stat: NicAttachmentState,
}
