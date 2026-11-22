// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::v1;

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
#[serde(rename_all = "snake_case")]
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

impl From<MigrationState> for v1::migration::MigrationState {
    fn from(new: MigrationState) -> Self {
        match new {
            MigrationState::Sync => Self::Sync,
            MigrationState::RamPush => Self::RamPush,
            MigrationState::Pause => Self::Pause,
            MigrationState::RamPushDirty => Self::RamPushDirty,
            MigrationState::Device => Self::Device,
            MigrationState::Resume => Self::Resume,
            MigrationState::RamPull => Self::RamPull,
            MigrationState::Server => Self::Server,
            MigrationState::Finish => Self::Finish,
            MigrationState::Error => Self::Error,
        }
    }
}

impl From<InstanceMigrationStatus> for v1::migration::InstanceMigrationStatus {
    fn from(new: InstanceMigrationStatus) -> Self {
        let InstanceMigrationStatus { id, state } = new;
        Self { id, state: state.into() }
    }
}

impl From<InstanceMigrateStatusResponse>
    for v1::migration::InstanceMigrateStatusResponse
{
    fn from(new: InstanceMigrateStatusResponse) -> Self {
        let InstanceMigrateStatusResponse { migration_in, migration_out } = new;
        Self {
            migration_in: migration_in.map(Into::into),
            migration_out: migration_out.map(Into::into),
        }
    }
}
