// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration types for the INITIAL API version.

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Request to initiate a migration.
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

/// Request to start a migration.
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
