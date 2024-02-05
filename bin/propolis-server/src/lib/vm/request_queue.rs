// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handles requests to change a Propolis server's state via the external API.
//!
//! An instance accepts or rejects requests to change state based on a
//! combination of its current state and its knowledge of the requests it has
//! previously queued but not processed yet. The latter knowledge is used to
//! reject requests that will never be fulfilled (because they're preceded by an
//! action that will forbid them; consider rebooting after stopping) or that may
//! need be to redirected to a migration target.
//!
//! The queue maintains a disposition for each kind of request that can be sent
//! to it, which allows that request to be enqueued, denied, or silently ignored
//! (for idempotency purposes). These dispositions can change as new requests
//! are queued. The queue also provides callbacks to the VM state driver that
//! allow the driver to advise the queue of state changes that further affect
//! what requests should be accepted.
//!
//! Users who want to share a queue must wrap it in the synchronization objects
//! of their choice.

use std::collections::VecDeque;

use slog::{debug, info, Logger};
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

    /// Resets all the VM's devices and CPUs, then starts the VM.
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
    #[error("Operation requires an active instance")]
    InstanceNotActive,

    #[error("Already migrating into this instance")]
    MigrationTargetInProgress,

    #[error("Instance is currently starting")]
    StartInProgress,

    #[error("Instance is already a migration source")]
    AlreadyMigrationSource,

    #[error("Operation cannot be performed on a migration source")]
    InvalidRequestForMigrationSource,

    #[error("Instance is preparing to stop")]
    HaltPending,

    #[error("Instance failed to start or halted due to a failure")]
    InstanceFailed,
}

/// The set of instance state changes that should change the dispositions of
/// future requests to the queue.
#[derive(Copy, Clone, Debug)]
pub enum InstanceStateChange {
    StartedRunning,
    Rebooted,
    Stopped,
    Failed,
}

/// A reason for a change in the queue's request dispositions.
#[derive(Debug)]
enum DispositionChangeReason<'a> {
    ApiRequest(&'a ExternalRequest),
    StateChange(InstanceStateChange),
}

/// The possible methods of handling a request to queue a state change.
#[derive(Copy, Clone, Debug)]
enum RequestDisposition {
    /// Put the state change on the queue.
    Enqueue,

    /// Drop the state change silently. This is used to make requests appear
    /// idempotent to callers without making the state driver deal with the
    /// consequences of queuing the same state change request twice.
    Ignore,

    /// Deny the request to change state.
    Deny(RequestDeniedReason),
}

/// The current disposition for each kind of incoming request.
#[derive(Copy, Clone, Debug)]
struct AllowedRequests {
    migrate_as_target: RequestDisposition,
    start: RequestDisposition,
    migrate_as_source: RequestDisposition,
    reboot: RequestDisposition,
    stop: RequestDisposition,
}

#[derive(Debug)]
pub struct ExternalRequestQueue {
    queue: VecDeque<ExternalRequest>,
    allowed: AllowedRequests,
    log: Logger,
}

impl ExternalRequestQueue {
    /// Creates a new queue that logs to the supplied logger.
    pub fn new(log: Logger) -> Self {
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
                stop: RequestDisposition::Enqueue,
            },
            log,
        }
    }

    /// Pops the request at the front of the queue.
    pub fn pop_front(&mut self) -> Option<ExternalRequest> {
        self.queue.pop_front()
    }

    /// Indicates whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Asks to place the supplied request on the queue. If the requests is
    /// enqueued, updates the dispositions to use for future requests.
    pub fn try_queue(
        &mut self,
        request: ExternalRequest,
    ) -> Result<(), RequestDeniedReason> {
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
            ExternalRequest::Stop => self.allowed.stop,
        };

        info!(&self.log, "Queuing external request";
              "request" => ?request,
              "disposition" => ?disposition);

        match disposition {
            RequestDisposition::Enqueue => {}
            RequestDisposition::Ignore => return Ok(()),
            RequestDisposition::Deny(reason) => return Err(reason),
        };

        self.allowed = self.get_new_dispositions(
            DispositionChangeReason::ApiRequest(&request),
        );
        self.queue.push_back(request);
        Ok(())
    }

    /// Notifies the queue that the instance's state has changed and that its
    /// disposition should be updated accordingly.
    pub fn notify_instance_state_change(&mut self, state: InstanceStateChange) {
        self.allowed = self
            .get_new_dispositions(DispositionChangeReason::StateChange(state));
    }

    /// Indicates whether the queue would allow a request to migrate into this
    /// instance. This can be used to avoid setting up migration tasks for
    /// requests that will ultimately be denied.
    ///
    /// # Return value
    ///
    /// - `Ok(true)` if the request will be queued.
    /// - `Ok(false)` if the request is allowed for idempotency reasons but will
    ///   not be queued.
    /// - `Err` if the request is forbidden.
    pub fn migrate_as_target_will_enqueue(
        &self,
    ) -> Result<bool, RequestDeniedReason> {
        match self.allowed.migrate_as_target {
            RequestDisposition::Enqueue => Ok(true),
            RequestDisposition::Ignore => Ok(false),
            RequestDisposition::Deny(reason) => Err(reason),
        }
    }

    /// Indicates whether the queue would allow a request to migrate out of this
    /// instance. This can be used to avoid setting up migration tasks for
    /// requests that will ultimately be denied.
    ///
    /// # Return value
    ///
    /// - `Ok(true)` if the request will be queued.
    /// - `Ok(false)` if the request is allowed for idempotency reasons but will
    ///   not be queued.
    /// - `Err` if the request is forbidden.
    pub fn migrate_as_source_will_enqueue(
        &self,
    ) -> Result<bool, RequestDeniedReason> {
        assert!(!matches!(
            self.allowed.migrate_as_source,
            RequestDisposition::Ignore
        ));

        match self.allowed.migrate_as_source {
            RequestDisposition::Enqueue => Ok(true),
            RequestDisposition::Ignore => unreachable!(),
            RequestDisposition::Deny(reason) => Err(reason),
        }
    }

    /// Computes a new set of queue dispositions given the current state of the
    /// queue and the event that is changing those dispositions.
    fn get_new_dispositions(
        &self,
        reason: DispositionChangeReason,
    ) -> AllowedRequests {
        debug!(self.log, "Computing new queue dispositions";
               "reason" => ?reason);

        use DispositionChangeReason as ChangeReason;
        use RequestDeniedReason as DenyReason;
        use RequestDisposition as Disposition;
        match reason {
            // Starting the instance, whether via migration or cold boot,
            // forecloses on further attempts to migrate in. For idempotency,
            // further requests to start are allowed when an instance-starting
            // transition is enqueued.
            ChangeReason::ApiRequest(ExternalRequest::MigrateAsTarget {
                ..
            })
            | ChangeReason::ApiRequest(ExternalRequest::Start) => {
                let (migrate_as_target_disposition, deny_reason) = match reason
                {
                    // If this is a request to migrate in, make sure future
                    // requests to migrate in are handled idempotently.
                    ChangeReason::ApiRequest(
                        ExternalRequest::MigrateAsTarget { .. },
                    ) => (
                        Disposition::Ignore,
                        DenyReason::MigrationTargetInProgress,
                    ),
                    ChangeReason::ApiRequest(ExternalRequest::Start) => (
                        Disposition::Deny(DenyReason::StartInProgress),
                        DenyReason::StartInProgress,
                    ),
                    _ => unreachable!(),
                };

                AllowedRequests {
                    migrate_as_target: migrate_as_target_disposition,
                    start: Disposition::Ignore,
                    migrate_as_source: Disposition::Deny(deny_reason),
                    reboot: Disposition::Deny(deny_reason),
                    stop: self.allowed.stop,
                }
            }
            ChangeReason::ApiRequest(ExternalRequest::MigrateAsSource {
                ..
            }) => {
                assert!(matches!(self.allowed.start, Disposition::Ignore));

                // Requests to migrate into the instance should not be enqueued
                // from this point, but whether they're dropped or ignored
                // depends on how the instance was originally initialized.
                assert!(!matches!(
                    self.allowed.migrate_as_target,
                    Disposition::Enqueue
                ));

                AllowedRequests {
                    migrate_as_target: self.allowed.migrate_as_target,
                    start: self.allowed.start,
                    migrate_as_source: Disposition::Deny(
                        DenyReason::AlreadyMigrationSource,
                    ),
                    reboot: Disposition::Deny(
                        DenyReason::InvalidRequestForMigrationSource,
                    ),
                    stop: self.allowed.stop,
                }
            }

            // Requests to reboot prevent additional reboot requests from being
            // queued, but do not affect other operations.
            ChangeReason::ApiRequest(ExternalRequest::Reboot) => {
                assert!(matches!(self.allowed.start, Disposition::Ignore));
                assert!(!matches!(
                    self.allowed.migrate_as_target,
                    Disposition::Enqueue
                ));

                AllowedRequests { reboot: Disposition::Ignore, ..self.allowed }
            }

            // Requests to stop the instance block other requests from being
            // queued. Additional requests to stop are ignored for idempotency.
            ChangeReason::ApiRequest(ExternalRequest::Stop) => {
                AllowedRequests {
                    migrate_as_target: Disposition::Deny(
                        DenyReason::HaltPending,
                    ),
                    start: Disposition::Deny(DenyReason::HaltPending),
                    migrate_as_source: Disposition::Deny(
                        DenyReason::HaltPending,
                    ),
                    reboot: Disposition::Deny(DenyReason::HaltPending),
                    stop: Disposition::Ignore,
                }
            }

            // When an instance begins running, requests to migrate out of it or
            // to reboot it become valid.
            ChangeReason::StateChange(InstanceStateChange::StartedRunning) => {
                AllowedRequests {
                    migrate_as_target: self.allowed.migrate_as_target,
                    start: self.allowed.start,
                    migrate_as_source: Disposition::Enqueue,
                    reboot: Disposition::Enqueue,
                    stop: self.allowed.stop,
                }
            }

            // When an instance finishes rebooting, allow new reboot requests to
            // be queued again, unless reboot requests began to be denied in the
            // meantime.
            ChangeReason::StateChange(InstanceStateChange::Rebooted) => {
                let new_reboot =
                    if let Disposition::Ignore = self.allowed.reboot {
                        Disposition::Enqueue
                    } else {
                        self.allowed.reboot
                    };

                AllowedRequests { reboot: new_reboot, ..self.allowed }
            }

            // When an instance stops or fails, requests to do anything other
            // than stop it are denied with an appropriate deny reason. Note
            // that an instance may stop or fail due to guest activity, so the
            // previous dispositions for migrate and reboot requests may not be
            // "deny".
            ChangeReason::StateChange(InstanceStateChange::Stopped) => {
                AllowedRequests {
                    migrate_as_target: Disposition::Deny(
                        DenyReason::InstanceNotActive,
                    ),
                    start: Disposition::Deny(DenyReason::InstanceNotActive),
                    migrate_as_source: Disposition::Deny(
                        DenyReason::InstanceNotActive,
                    ),
                    reboot: Disposition::Deny(DenyReason::InstanceNotActive),
                    stop: Disposition::Ignore,
                }
            }
            ChangeReason::StateChange(InstanceStateChange::Failed) => {
                AllowedRequests {
                    migrate_as_target: Disposition::Deny(
                        DenyReason::InstanceFailed,
                    ),
                    start: Disposition::Deny(DenyReason::InstanceFailed),
                    migrate_as_source: Disposition::Deny(
                        DenyReason::InstanceFailed,
                    ),
                    reboot: Disposition::Deny(DenyReason::InstanceFailed),
                    stop: self.allowed.stop,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use uuid::Uuid;

    fn test_logger() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn make_migrate_as_target_request() -> ExternalRequest {
        let task = tokio::task::spawn(async { Ok(()) });
        let (start_tx, _) = tokio::sync::oneshot::channel();
        let (_, command_rx) = tokio::sync::mpsc::channel(1);
        ExternalRequest::MigrateAsTarget {
            migration_id: Uuid::new_v4(),
            task,
            start_tx,
            command_rx,
        }
    }

    fn make_migrate_as_source_request() -> ExternalRequest {
        let task = tokio::task::spawn(async { Ok(()) });
        let (start_tx, _) = tokio::sync::oneshot::channel();
        let (_, command_rx) = tokio::sync::mpsc::channel(1);
        let (response_tx, _) = tokio::sync::mpsc::channel(1);
        ExternalRequest::MigrateAsSource {
            migration_id: Uuid::new_v4(),
            task,
            start_tx,
            command_rx,
            response_tx,
        }
    }

    #[tokio::test]
    async fn migrate_as_target_is_idempotent() {
        let mut queue = ExternalRequestQueue::new(test_logger());

        // Requests to migrate as a target should queue normally at first.
        assert!(queue.migrate_as_target_will_enqueue().unwrap());

        // After queuing such a request, subsequent requests should be allowed
        // without enqueuing anything.
        assert!(queue.try_queue(make_migrate_as_target_request()).is_ok());
        assert!(!queue.migrate_as_target_will_enqueue().unwrap());

        // Pop the request and tell the queue the instance is running.
        assert!(matches!(
            queue.pop_front(),
            Some(ExternalRequest::MigrateAsTarget { .. })
        ));
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);

        // Because the instance was started via migration in, future requests
        // to migrate in should be allowed.
        assert!(queue.try_queue(make_migrate_as_target_request()).is_ok());
        assert!(!queue.migrate_as_target_will_enqueue().unwrap());
    }

    #[tokio::test]
    async fn migrate_as_target_is_forbidden_after_cold_boot() {
        let mut queue = ExternalRequestQueue::new(test_logger());
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);

        assert!(queue.migrate_as_target_will_enqueue().is_err());
        assert!(queue.try_queue(make_migrate_as_target_request()).is_err());
    }

    #[tokio::test]
    async fn migrate_as_source_is_not_idempotent() {
        // Simulate a running instance.
        let mut queue = ExternalRequestQueue::new(test_logger());
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Start)));
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);

        // Requests to migrate out should be allowed.
        assert!(queue.migrate_as_source_will_enqueue().unwrap());
        assert!(queue.try_queue(make_migrate_as_source_request()).is_ok());

        // Once the request is queued, other requests to migrate out are
        // disallowed until the queued request is disposed of.
        //
        // This differs from the migration-in case in that requests to migrate
        // in are issued by the sled agent as part of a saga (where idempotency
        // is assumed), but requests to migrate out are issued by the target
        // Propolis (which does not assume idempotency and issues only one
        // request per migration attempt).
        assert!(queue.migrate_as_source_will_enqueue().is_err());
        assert!(queue.try_queue(make_migrate_as_source_request()).is_err());

        // If migration fails, the instance resumes running, and then another
        // request to migrate out should be allowed.
        assert!(matches!(
            queue.pop_front(),
            Some(ExternalRequest::MigrateAsSource { .. })
        ));
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);
        assert!(queue.migrate_as_source_will_enqueue().unwrap());
        assert!(queue.try_queue(make_migrate_as_source_request()).is_ok());

        // A successful migration stops the instance, which forecloses on future
        // requests to migrate out.
        queue.pop_front();
        queue.notify_instance_state_change(InstanceStateChange::Stopped);
        assert!(queue.migrate_as_source_will_enqueue().is_err());
        assert!(queue.try_queue(make_migrate_as_source_request()).is_err());
    }

    #[tokio::test]
    async fn stop_requests_enqueue_after_vm_failure() {
        let mut queue = ExternalRequestQueue::new(test_logger());
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Start)));
        queue.notify_instance_state_change(InstanceStateChange::Failed);

        assert!(queue.try_queue(ExternalRequest::Stop).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Stop)));
    }

    #[tokio::test]
    async fn reboot_requests_are_idempotent_except_when_stopping() {
        let mut queue = ExternalRequestQueue::new(test_logger());
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Start)));
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);

        // Once the instance is started, reboot requests should be allowed, but
        // after the first, subsequent requests should be dropped for
        // idempotency.
        assert!(queue.is_empty());
        for _ in 0..5 {
            assert!(queue.try_queue(ExternalRequest::Reboot).is_ok());
        }
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Reboot)));
        assert!(queue.is_empty());

        // Once the instance has rebooted, new requests can be queued.
        queue.notify_instance_state_change(InstanceStateChange::Rebooted);
        assert!(queue.try_queue(ExternalRequest::Reboot).is_ok());
        assert!(!queue.is_empty());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Reboot)));
        queue.notify_instance_state_change(InstanceStateChange::Rebooted);

        // If a request to reboot is queued, and then a request to stop is
        // queued, new requests to reboot should always fail, even after the
        // instance finishes rebooting.
        assert!(queue.try_queue(ExternalRequest::Reboot).is_ok());
        assert!(!queue.is_empty());
        assert!(queue.try_queue(ExternalRequest::Stop).is_ok());
        assert!(queue.try_queue(ExternalRequest::Reboot).is_err());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Reboot)));
        queue.notify_instance_state_change(InstanceStateChange::Rebooted);
        assert!(queue.try_queue(ExternalRequest::Reboot).is_err());
    }
}
