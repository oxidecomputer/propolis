// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helper types for publishing instance states as made visible through the
//! external API.

use propolis_api_types::{
    InstanceMigrateStatusResponse, InstanceMigrationStatus, InstanceState,
    InstanceStateMonitorResponse,
};
use slog::info;
use uuid::Uuid;

use crate::migrate::MigrateRole;

use super::{InstanceStateRx, InstanceStateTx};

/// An update to an instance's migration's state.
pub(super) struct MigrationStateUpdate {
    /// The migration's new state.
    pub state: propolis_api_types::MigrationState,

    /// The migration's ID.
    pub id: Uuid,

    /// The role this VM was playing in the migration of interest.
    pub role: MigrateRole,
}

impl MigrationStateUpdate {
    /// Applies an update to a previous migration status and returns the new
    /// status.
    fn apply_to(
        self,
        old: InstanceMigrateStatusResponse,
    ) -> InstanceMigrateStatusResponse {
        let new = InstanceMigrationStatus { id: self.id, state: self.state };
        match self.role {
            MigrateRole::Destination => InstanceMigrateStatusResponse {
                migration_in: Some(new),
                migration_out: old.migration_out,
            },
            MigrateRole::Source => InstanceMigrateStatusResponse {
                migration_in: old.migration_in,
                migration_out: Some(new),
            },
        }
    }
}

/// A kind of state update to publish.
pub(super) enum ExternalStateUpdate {
    /// Update the instance state (but not any migration state).
    Instance(InstanceState),

    /// Update migration state (but not the instance's state).
    Migration(MigrationStateUpdate),

    /// Update both instance and migration state.
    Complete(InstanceState, MigrationStateUpdate),
}

/// A channel to which to publish externally-visible instance state updates.
pub(super) struct StatePublisher {
    tx: InstanceStateTx,
    log: slog::Logger,
}

impl StatePublisher {
    pub(super) fn new(
        log: &slog::Logger,
        initial_state: InstanceStateMonitorResponse,
    ) -> (Self, InstanceStateRx) {
        let (tx, rx) = tokio::sync::watch::channel(initial_state);
        (Self { tx, log: log.clone() }, rx)
    }

    /// Updates an instance's externally-visible state and publishes that state
    /// with a successor generation number.
    pub(super) fn update(&mut self, update: ExternalStateUpdate) {
        let (instance_state, migration_state) = match update {
            ExternalStateUpdate::Instance(i) => (Some(i), None),
            ExternalStateUpdate::Migration(m) => (None, Some(m)),
            ExternalStateUpdate::Complete(i, m) => (Some(i), Some(m)),
        };

        let InstanceStateMonitorResponse {
            state: old_instance,
            migration: old_migration,
            gen: old_gen,
        } = self.tx.borrow().clone();

        let state = instance_state.unwrap_or(old_instance);
        let migration = if let Some(migration_state) = migration_state {
            migration_state.apply_to(old_migration)
        } else {
            old_migration
        };

        let gen = old_gen + 1;
        info!(self.log, "publishing new instance state";
              "gen" => gen,
              "state" => ?state,
              "migration" => ?migration);

        let _ =
            self.tx.send(propolis_api_types::InstanceStateMonitorResponse {
                gen,
                state,
                migration,
            });
    }
}
