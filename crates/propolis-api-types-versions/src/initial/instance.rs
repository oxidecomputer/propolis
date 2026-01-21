// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Instance management types for the INITIAL API version.
//!
//! This module contains types for instance properties, state management,
//! initialization, and monitoring.

use std::{collections::BTreeMap, net::SocketAddr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::components::{backends, devices};
use super::instance_spec::{InstanceSpec, SpecKey};
use super::migration::InstanceMigrateInitiateResponse;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct InstanceMetadata {
    pub silo_id: Uuid,
    pub project_id: Uuid,
    pub sled_id: Uuid,
    pub sled_serial: String,
    pub sled_revision: u32,
    pub sled_model: String,
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Instance {
    pub properties: InstanceProperties,
    pub state: InstanceState,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceGetResponse {
    pub instance: Instance,
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

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorRequest {
    pub gen: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstanceStateMonitorResponse {
    pub gen: u64,
    pub state: InstanceState,
    pub migration: super::migration::InstanceMigrateStatusResponse,
}

/// An instance spec component that should be replaced during a live migration.
//
// When a caller asks Propolis to initialize via live migration, the target VM
// inherits the migration source's current instance spec. For the most part,
// the target can (and indeed in some cases must) use this spec without
// modifying it; this helps Propolis ensure that guest-visible configuration
// remains unchanged when a VM migrates. However, there are some components
// with no guest-visible state that may need to be reconfigured when a VM
// migrates. These include the following:
//
// - Crucible disks: After migrating, the target Propolis presents itself as a
//   new client of the Crucible downstairs servers backing the VM's disks.
//   Crucible requires the target to present a newer client generation number
//   to allow the target to connect. In a full Oxide deployment, these numbers
//   are managed by the control plane (i.e. it is not safe for Propolis to
//   manage these values directly--new Crucible volume connection information
//   must always come from Nexus).
// - Virtio network devices: Each virtio NIC in the guest needs to bind to a
//   named VNIC object on the host. These names can change when a VM migrates
//   from host to host.
//
// Each component that can be reconfigured this way has a variant in this enum;
// components not in the enum can't be reconfigured during migration. This
// saves the initialization API from having to reason about requests to replace
// a component that can't legally be replaced.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, tag = "component", content = "spec")]
pub enum ReplacementComponent {
    MigrationFailureInjector(devices::MigrationFailureInjector),
    CrucibleStorageBackend(backends::CrucibleStorageBackend),
    VirtioNetworkBackend(backends::VirtioNetworkBackend),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "method", content = "value")]
pub enum InstanceInitializationMethod {
    Spec {
        spec: InstanceSpec,
    },
    MigrationTarget {
        migration_id: Uuid,
        src_addr: SocketAddr,
        replace_components: BTreeMap<SpecKey, ReplacementComponent>,
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

/// Path parameters for instance-related endpoints using name.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstanceNameParams {
    pub instance_id: String,
}

/// Path parameters for instance-related endpoints using UUID.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct InstancePathParams {
    pub instance_id: Uuid,
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
