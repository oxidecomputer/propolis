#![allow(dead_code)]

use std::collections::VecDeque;

use thiserror::Error;
use uuid::Uuid;

use crate::migrate::MigrateError;

use super::{
    MigrateSourceCommand, MigrateSourceResponse, MigrateTargetCommand,
};

/// An external request made of a VM controller via the server API. Handled by
/// the controller's state driver thread.
#[derive(Debug)]
pub enum ExternalRequest {
    /// Initializes the VM through live migration by running a
    /// migration-destination task.
    MigrateAsTarget {
        /// The ID of the live migration to use when initializing.
        migration_id: Uuid,

        /// A handle to the task that will execute the migration procedure.
        task: tokio::task::JoinHandle<Result<(), MigrateError>>,

        /// The sender side of a one-shot channel that, when signaled, tells the
        /// migration task to start its work.
        start_tx: tokio::sync::oneshot::Sender<()>,

        /// A channel that receives commands from the migration task.
        command_rx: tokio::sync::mpsc::Receiver<MigrateTargetCommand>,
    },

    /// Resets all the VM's entities and CPUs, then starts the VM.
    Start,

    /// Asks the state worker to start a migration-source task.
    MigrateAsSource {
        /// The ID of the live migration for which this VM will be the source.
        migration_id: Uuid,

        /// A handle to the task that will execute the migration procedure.
        task: tokio::task::JoinHandle<Result<(), MigrateError>>,

        /// The sender side of a one-shot channel that, when signaled, tells the
        /// migration task to start its work.
        start_tx: tokio::sync::oneshot::Sender<()>,

        /// A channel that receives commands from the migration task.
        command_rx: tokio::sync::mpsc::Receiver<MigrateSourceCommand>,

        /// A channel used to send responses to migration commands.
        response_tx: tokio::sync::mpsc::Sender<MigrateSourceResponse>,
    },

    /// Resets the guest by pausing all devices, resetting them to their
    /// cold-boot states, and resuming the devices. Note that this is not a
    /// graceful reboot and does not coordinate with guest software.
    Reboot,

    /// Halts the VM. Note that this is not a graceful shutdown and does not
    /// coordinate with guest software.
    Stop,
}

/// A set of reasons why a request to queue an external state transition can
/// fail.
#[derive(Copy, Clone, Debug, Error)]
pub enum RequestDeniedReason {
    #[error("The requested operation requires an active instance")]
    InstanceNotActive,

    #[error("A migration into this instance is in progress")]
    MigrationTargetInProgress,

    #[error("The instance is currently starting")]
    StartInProgress,

    #[error("The instance is already a migration source")]
    AlreadyMigrationSource,

    #[error(
        "The requested operation cannot be performed on a migration source"
    )]
    InvalidRequestForMigrationSource,

    #[error("The instance is preparing to stop")]
    HaltPending,
}

#[derive(Copy, Clone, Debug)]
pub enum RequestDisposition {
    Enqueue,
    Ignore,
    Deny(RequestDeniedReason),
}

#[derive(Debug)]
pub struct AllowedRequests {
    pub migrate_as_target: RequestDisposition,
    pub start: RequestDisposition,
    pub migrate_as_source: RequestDisposition,
    pub reboot: RequestDisposition,
}

#[derive(Debug)]
pub struct ExternalRequestQueue {
    queue: VecDeque<ExternalRequest>,
    allowed: AllowedRequests,
}

impl ExternalRequestQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            allowed: AllowedRequests {
                migrate_as_target: RequestDisposition::Enqueue,
                start: RequestDisposition::Enqueue,
                migrate_as_source: RequestDisposition::Deny(
                    RequestDeniedReason::InstanceNotActive,
                ),
                reboot: RequestDisposition::Deny(
                    RequestDeniedReason::InstanceNotActive,
                ),
            },
        }
    }

    pub fn pop_front(&mut self) -> Option<ExternalRequest> {
        self.queue.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn try_queue(
        &mut self,
        request: ExternalRequest,
    ) -> Result<(), RequestDeniedReason> {
        use RequestDeniedReason as DenyReason;
        use RequestDisposition as Disposition;

        let disposition = match request {
            ExternalRequest::MigrateAsTarget { .. } => {
                self.allowed.migrate_as_target
            }
            ExternalRequest::Start => self.allowed.start,
            ExternalRequest::MigrateAsSource { .. } => {
                self.allowed.migrate_as_source
            }
            ExternalRequest::Reboot => self.allowed.reboot,

            // Requests to stop always succeed. Note that a request to stop a VM
            // that hasn't started should still be queued to the state worker so
            // that the worker can exit and drop its references to the instance.
            ExternalRequest::Stop => Disposition::Enqueue,
        };

        match disposition {
            Disposition::Enqueue => {}
            Disposition::Ignore => return Ok(()),
            Disposition::Deny(reason) => return Err(reason),
        };

        // At this point the request will be queued. Queuing some requests
        // logically forecloses on other kinds of requests. Update the
        // dispositions of these requests, then queue the request.
        match request {
            // Starting the instance, whether via migration or cold boot,
            // forecloses on further attempts to migrate in. For idempotency,
            // further requests to start are allowed when an instance-starting
            // transition is enqueued.
            ExternalRequest::MigrateAsTarget { .. }
            | ExternalRequest::Start => {
                let deny_reason = match request {
                    ExternalRequest::MigrateAsTarget { .. } => {
                        DenyReason::MigrationTargetInProgress
                    }
                    ExternalRequest::Start => DenyReason::StartInProgress,
                    _ => unreachable!(),
                };

                self.allowed.start = Disposition::Ignore;
                self.allowed.migrate_as_target = Disposition::Deny(deny_reason);
                self.allowed.reboot = Disposition::Deny(deny_reason);
                self.allowed.migrate_as_source = Disposition::Deny(deny_reason);
            }

            // Acting as a migration source forbids new migrations from
            // starting. It also forbids requests to reboot, since after a
            // successful migration out these should instead be handled by the
            // migration target.
            //
            // If migrating as a source is allowed, migrating as a target should
            // be forbidden, and requests to run should be idempotently
            // accepted.
            ExternalRequest::MigrateAsSource { .. } => {
                assert!(matches!(
                    self.allowed.migrate_as_target,
                    Disposition::Deny(_)
                ));
                assert!(matches!(self.allowed.start, Disposition::Ignore));
                self.allowed.migrate_as_source =
                    Disposition::Deny(DenyReason::AlreadyMigrationSource);
                self.allowed.reboot = Disposition::Deny(
                    DenyReason::InvalidRequestForMigrationSource,
                );
            }

            // Requests to reboot don't affect whether operations can be
            // performed.
            ExternalRequest::Reboot => {
                assert!(matches!(
                    self.allowed.migrate_as_target,
                    Disposition::Deny(_)
                ));
                assert!(matches!(self.allowed.start, Disposition::Ignore));
            }

            // Queueing a request to stop an instance disables any other
            // operations on that instance.
            ExternalRequest::Stop => {
                self.allowed.migrate_as_target =
                    Disposition::Deny(DenyReason::HaltPending);
                self.allowed.start = Disposition::Deny(DenyReason::HaltPending);
                self.allowed.migrate_as_source =
                    Disposition::Deny(DenyReason::HaltPending);
                self.allowed.reboot =
                    Disposition::Deny(DenyReason::HaltPending);
            }
        }

        self.queue.push_back(request);
        Ok(())
    }

    pub fn set_allowed_requests(&mut self, new_allowed: AllowedRequests) {
        self.allowed = new_allowed;
    }

    pub fn migrate_as_target_allowed(&self) -> Result<(), RequestDeniedReason> {
        match self.allowed.migrate_as_target {
            RequestDisposition::Enqueue => Ok(()),
            RequestDisposition::Ignore => {
                panic!("requests to migrate as target should not be ignored")
            }
            RequestDisposition::Deny(reason) => Err(reason),
        }
    }

    pub fn migrate_as_source_allowed(&self) -> Result<(), RequestDeniedReason> {
        match self.allowed.migrate_as_target {
            RequestDisposition::Enqueue => Ok(()),
            RequestDisposition::Ignore => {
                panic!("requests to migrate as source should not be ignored")
            }
            RequestDisposition::Deny(reason) => Err(reason),
        }
    }
}
