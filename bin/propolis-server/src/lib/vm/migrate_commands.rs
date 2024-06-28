// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Requests and responses between the VM state driver and the live migration
//! protocol.

use std::sync::Arc;

use crate::migrate::MigrateError;

/// A message sent from a live migration destination task to update the
/// externally visible state of the migration attempt.
#[derive(Clone, Copy, Debug)]
pub enum MigrateTargetCommand {
    /// Initialize VM objects using the instance spec supplied to the state
    /// driver by its creator.
    InitializeFromExternalSpec,

    /// Update the externally-visible migration state.
    UpdateState(propolis_api_types::MigrationState),
}

#[derive(Clone)]
pub enum MigrateTargetResponse {
    VmObjectsInitialized(Result<Arc<crate::vm::objects::VmObjects>, String>),
}

/// A message sent from a live migration driver to the state worker, asking it
/// to act on source instance components on the task's behalf.
#[derive(Clone, Copy, Debug)]
pub enum MigrateSourceCommand {
    /// Update the externally-visible migration state.
    UpdateState(propolis_api_types::MigrationState),

    /// Determine whether a previous attempt to restore the VM's dirty bitmap
    /// has failed.
    QueryRedirtyingFailed,

    /// Record that the guest's dirty page bitmap may be inconsistent so that
    /// future attempts to migrate out transmit all pages.
    RedirtyingFailed,

    /// Pause the instance's devices and CPUs.
    Pause,
}

/// A message sent from the state worker to the live migration driver in
/// response to a previous command.
#[derive(Debug)]
pub enum MigrateSourceResponse {
    /// A previous migration out has (or has not) failed to restore the VM's
    /// dirty bitmap.
    RedirtyingFailed(bool),

    /// A request to pause completed with the attached result.
    Pause(Result<(), std::io::Error>),
}

/// An event raised by a migration task that must be handled by the state
/// worker.
#[derive(Debug)]
pub(super) enum MigrateTaskEvent<T> {
    /// The task completed with the associated result.
    TaskExited(Result<(), MigrateError>),

    /// The task sent a command requesting work.
    Command(T),
}

pub(super) async fn next_migrate_task_event<E>(
    task: &mut tokio::task::JoinHandle<
        Result<(), crate::migrate::MigrateError>,
    >,
    command_rx: &mut tokio::sync::mpsc::Receiver<E>,
    log: &slog::Logger,
) -> MigrateTaskEvent<E> {
    if let Some(cmd) = command_rx.recv().await {
        return MigrateTaskEvent::Command(cmd);
    }

    // The sender side of the command channel is dropped, which means the
    // migration task is exiting. Wait for it to finish and snag its result.
    match task.await {
        Ok(res) => {
            slog::info!(log, "Migration task exited: {:?}", res);
            MigrateTaskEvent::TaskExited(res)
        }
        Err(join_err) => {
            if join_err.is_cancelled() {
                panic!("Migration task canceled");
            } else {
                panic!("Migration task panicked: {:?}", join_err.into_panic());
            }
        }
    }
}
