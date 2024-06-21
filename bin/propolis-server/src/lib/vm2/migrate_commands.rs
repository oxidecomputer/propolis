// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Requests and responses between the VM state driver and the live migration
//! protocol.

use crate::migrate::MigrateError;

/// A message sent from a live migration destination task to update the
/// externally visible state of the migration attempt.
#[derive(Clone, Copy, Debug)]
pub enum MigrateTargetCommand {
    /// Update the externally-visible migration state.
    UpdateState(propolis_api_types::MigrationState),
}

/// A message sent from a live migration driver to the state worker, asking it
/// to act on source instance components on the task's behalf.
#[derive(Clone, Copy, Debug)]
pub enum MigrateSourceCommand {
    /// Update the externally-visible migration state.
    UpdateState(propolis_api_types::MigrationState),

    /// Pause the instance's devices and CPUs.
    Pause,
}

/// A message sent from the state worker to the live migration driver in
/// response to a previous command.
#[derive(Debug)]
pub enum MigrateSourceResponse {
    /// A request to pause completed with the attached result.
    Pause(Result<(), std::io::Error>),
}

/// An event raised by a migration task that must be handled by the state
/// worker.
#[derive(Debug)]
enum MigrateTaskEvent<T> {
    /// The task completed with the associated result.
    TaskExited(Result<(), MigrateError>),

    /// The task sent a command requesting work.
    Command(T),
}