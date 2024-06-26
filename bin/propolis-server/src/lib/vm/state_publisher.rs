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

pub(super) struct MigrationStateUpdate {
    pub state: propolis_api_types::MigrationState,
    pub id: Uuid,
    pub role: MigrateRole,
}

impl MigrationStateUpdate {
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

pub(super) enum ExternalStateUpdate {
    Instance(InstanceState),
    Migration(MigrationStateUpdate),
    Complete(InstanceState, MigrationStateUpdate),
}

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
