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

use propolis_api_types::instance_spec::SpecKey;
use slog::{debug, info, Logger};
use thiserror::Error;
use uuid::Uuid;

/// Wraps a [`dropshot::WebsocketConnection`] for inclusion in an
/// [`ExternalRequest`].
//
// This newtype allows this module's tests (which want to verify queuing
// dispositions and don't care about request contents) to construct a
// `MigrateAsSource` request without having to conjure up a real websocket
// connection.
pub(crate) struct WebsocketConnection(Option<dropshot::WebsocketConnection>);

impl From<dropshot::WebsocketConnection> for WebsocketConnection {
    fn from(value: dropshot::WebsocketConnection) -> Self {
        Self(Some(value))
    }
}

impl WebsocketConnection {
    /// Yields the wrapped [`dropshot::WebsocketConnection`].
    pub(crate) fn into_inner(self) -> dropshot::WebsocketConnection {
        // Unwrapping is safe here because the only way an external consumer can
        // get an instance of this wrapper is to use the From impl, which always
        // wraps a `Some`.
        self.0.unwrap()
    }
}

/// An external request made of a VM controller via the server API. Handled by
/// the controller's state driver thread.
pub enum ExternalRequest {
    /// Asks the state worker to start a brand-new VM (i.e. not one initialized
    /// by live migration, which implicitly starts the VM).
    Start,

    /// Asks the state worker to start a migration-source task.
    MigrateAsSource { migration_id: Uuid, websock: WebsocketConnection },

    /// Resets the guest by pausing all devices, resetting them to their
    /// cold-boot states, and resuming the devices. Note that this is not a
    /// graceful reboot and does not coordinate with guest software.
    Reboot,

    /// Halts the VM. Note that this is not a graceful shutdown and does not
    /// coordinate with guest software.
    Stop,

    /// Attempts to update the volume construction request for the supplied
    /// Crucible volume.
    ReconfigureCrucibleVolume {
        /// The ID of the Crucible backend in the VM's Crucible backend map.
        backend_id: SpecKey,

        /// The new volume construction request to supply to the Crucible
        /// upstairs.
        new_vcr_json: String,

        /// The sink for the result of this operation.
        result_tx: super::CrucibleReplaceResultTx,
    },
}

impl std::fmt::Debug for ExternalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start => write!(f, "Start"),
            Self::MigrateAsSource { migration_id, .. } => f
                .debug_struct("MigrateAsSource")
                .field("migration_id", migration_id)
                .finish(),
            Self::Reboot => write!(f, "Reboot"),
            Self::Stop => write!(f, "Stop"),
            Self::ReconfigureCrucibleVolume { backend_id, .. } => f
                .debug_struct("ReconfigureCrucibleVolume")
                .field("backend_id", backend_id)
                .finish(),
        }
    }
}

/// A set of reasons why a request to queue an external state transition can
/// fail.
#[derive(Copy, Clone, Debug, Error)]
pub enum RequestDeniedReason {
    #[error("Operation requires an active instance")]
    InstanceNotActive,

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
    start: RequestDisposition,
    migrate_as_source: RequestDisposition,
    reboot: RequestDisposition,
    reconfigure_crucible_volume: RequestDisposition,
    stop: RequestDisposition,
}

/// A queue for external requests to change an instance's state.
#[derive(Debug)]
pub struct ExternalRequestQueue {
    queue: VecDeque<ExternalRequest>,
    allowed: AllowedRequests,
    log: Logger,
}

/// Indicates whether this queue's creator will start the relevant instance
/// without waiting for a Start request from the queue.
pub enum InstanceAutoStart {
    Yes,
    No,
}

impl ExternalRequestQueue {
    /// Creates a new queue that logs to the supplied logger.
    pub fn new(log: Logger, auto_start: InstanceAutoStart) -> Self {
        // If the queue is being created for an instance that will start
        // automatically (e.g. due to a migration in), set the request
        // disposition for future start requests to Ignore for idempotency.
        let start = match auto_start {
            InstanceAutoStart::Yes => RequestDisposition::Ignore,
            InstanceAutoStart::No => RequestDisposition::Enqueue,
        };

        Self {
            queue: VecDeque::new(),
            allowed: AllowedRequests {
                start,
                migrate_as_source: RequestDisposition::Deny(
                    RequestDeniedReason::InstanceNotActive,
                ),
                reboot: RequestDisposition::Deny(
                    RequestDeniedReason::InstanceNotActive,
                ),
                reconfigure_crucible_volume: RequestDisposition::Enqueue,
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
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Asks to place the supplied request on the queue. If the request is
    /// enqueued, updates the dispositions to use for future requests.
    pub fn try_queue(
        &mut self,
        request: ExternalRequest,
    ) -> Result<(), RequestDeniedReason> {
        let disposition = match request {
            ExternalRequest::Start => self.allowed.start,
            ExternalRequest::MigrateAsSource { .. } => {
                self.allowed.migrate_as_source
            }
            ExternalRequest::Reboot => self.allowed.reboot,
            ExternalRequest::ReconfigureCrucibleVolume { .. } => {
                self.allowed.reconfigure_crucible_volume
            }

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
            ChangeReason::ApiRequest(ExternalRequest::Start) => {
                let reason = DenyReason::StartInProgress;
                AllowedRequests {
                    start: Disposition::Ignore,
                    migrate_as_source: Disposition::Deny(reason),
                    reboot: Disposition::Deny(reason),
                    reconfigure_crucible_volume: Disposition::Enqueue,
                    stop: self.allowed.stop,
                }
            }
            ChangeReason::ApiRequest(ExternalRequest::MigrateAsSource {
                ..
            }) => {
                assert!(
                    matches!(self.allowed.start, Disposition::Ignore),
                    "{:?}",
                    self.allowed
                );

                AllowedRequests {
                    start: self.allowed.start,
                    migrate_as_source: Disposition::Deny(
                        DenyReason::AlreadyMigrationSource,
                    ),
                    reboot: Disposition::Deny(
                        DenyReason::InvalidRequestForMigrationSource,
                    ),
                    reconfigure_crucible_volume: Disposition::Deny(
                        DenyReason::InvalidRequestForMigrationSource,
                    ),
                    stop: self.allowed.stop,
                }
            }

            // Requests to reboot prevent additional reboot requests from being
            // queued, but do not affect other operations.
            ChangeReason::ApiRequest(ExternalRequest::Reboot) => {
                assert!(
                    matches!(self.allowed.start, Disposition::Ignore),
                    "{:?}",
                    self.allowed
                );
                AllowedRequests { reboot: Disposition::Ignore, ..self.allowed }
            }

            // Once the instance is asked to stop, further requests to change
            // its state are ignored.
            //
            // Requests to change Crucible volume configuration can still be
            // queued if they were previously alloewd. This allows the state
            // driver to accept VCR mutations that are needed to allow an
            // activation to complete even if the instance is slated to stop
            // immediately after starting.
            //
            // Note that it is possible for a VCR change request to be enqueued
            // and then not processed before the state driver stops reading from
            // the queue. If this happens, the request will be marked as failed
            // when the request queue containing it is dropped. (This can also
            // happen if a guest asks to halt a VM while it has a replacement
            // request on its queue.)
            ChangeReason::ApiRequest(ExternalRequest::Stop) => {
                let reason = DenyReason::HaltPending;
                AllowedRequests {
                    start: Disposition::Deny(reason),
                    migrate_as_source: Disposition::Deny(reason),
                    reboot: Disposition::Deny(reason),
                    reconfigure_crucible_volume: self
                        .allowed
                        .reconfigure_crucible_volume,
                    stop: Disposition::Ignore,
                }
            }

            // Requests to mutate VM configuration don't move the VM state
            // machine and don't change any request dispositions.
            ChangeReason::ApiRequest(
                ExternalRequest::ReconfigureCrucibleVolume { .. },
            ) => self.allowed,

            // When an instance begins running, requests to migrate out of it or
            // to reboot it become valid.
            ChangeReason::StateChange(InstanceStateChange::StartedRunning) => {
                AllowedRequests {
                    start: self.allowed.start,
                    migrate_as_source: Disposition::Enqueue,
                    reboot: Disposition::Enqueue,
                    reconfigure_crucible_volume: Disposition::Enqueue,
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
                let reason = DenyReason::InstanceNotActive;
                AllowedRequests {
                    start: Disposition::Deny(reason),
                    migrate_as_source: Disposition::Deny(reason),
                    reboot: Disposition::Deny(reason),
                    reconfigure_crucible_volume: Disposition::Deny(reason),
                    stop: Disposition::Ignore,
                }
            }
            ChangeReason::StateChange(InstanceStateChange::Failed) => {
                let reason = DenyReason::InstanceFailed;
                AllowedRequests {
                    start: Disposition::Deny(reason),
                    migrate_as_source: Disposition::Deny(reason),
                    reboot: Disposition::Deny(reason),
                    reconfigure_crucible_volume: Disposition::Deny(reason),
                    stop: self.allowed.stop,
                }
            }
        }
    }
}

// It's possible for an external request queue to be dropped with outstanding
// requests if an event from the guest shuts down the VM before the queue can be
// drained. If this happens, notify anyone waiting on a specific request on the
// queue that the VM is gone.
impl Drop for ExternalRequestQueue {
    fn drop(&mut self) {
        for req in self.queue.drain(..) {
            match req {
                // Crucible VCR change requestors wait for their requests to be
                // retired.
                ExternalRequest::ReconfigureCrucibleVolume {
                    result_tx,
                    ..
                } => {
                    let _ =
                        result_tx.send(Err(dropshot::HttpError::for_status(
                            Some(
                                "VM destroyed before request could be handled"
                                    .to_string(),
                            ),
                            hyper::StatusCode::GONE,
                        )));
                }

                // Requests to start, reboot, and stop are handled
                // asynchronously (calls to change the instance's state return
                // as soon as they're queued).
                ExternalRequest::Start
                | ExternalRequest::Reboot
                | ExternalRequest::Stop => {}

                // Dropping a request to migrate out drops the embedded
                // connection to the migration target, thus notifying it that
                // the source is gone.
                ExternalRequest::MigrateAsSource { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use uuid::Uuid;

    fn test_logger() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn make_migrate_as_source_request() -> ExternalRequest {
        ExternalRequest::MigrateAsSource {
            migration_id: Uuid::new_v4(),
            websock: WebsocketConnection(None),
        }
    }

    fn make_reconfigure_crucible_request() -> ExternalRequest {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        ExternalRequest::ReconfigureCrucibleVolume {
            backend_id: SpecKey::Uuid(Uuid::new_v4()),
            new_vcr_json: "".to_string(),
            result_tx: tx,
        }
    }

    #[tokio::test]
    async fn start_requests_become_idempotent_after_first_request() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::No);

        // The first request to start should succeed.
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());

        // The second one should too, but only for idempotency: the queue should
        // then have only one start request on it.
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Start)));
        assert!(queue.pop_front().is_none());

        // Start requests continue to be ignored even after the instance starts
        // to run.
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        assert!(queue.pop_front().is_none());
    }

    #[tokio::test]
    async fn migrate_as_source_is_not_idempotent() {
        // Simulate a running instance.
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);

        // Requests to migrate out should be allowed.
        assert!(queue.try_queue(make_migrate_as_source_request()).is_ok());

        // Once the request is queued, other requests to migrate out are
        // disallowed until the queued request is disposed of.
        //
        // This differs from the migration-in case in that requests to migrate
        // in are issued by the sled agent as part of a saga (where idempotency
        // is assumed), but requests to migrate out are issued by the target
        // Propolis (which does not assume idempotency and issues only one
        // request per migration attempt).
        assert!(queue.try_queue(make_migrate_as_source_request()).is_err());

        // If migration fails, the instance resumes running, and then another
        // request to migrate out should be allowed.
        assert!(matches!(
            queue.pop_front(),
            Some(ExternalRequest::MigrateAsSource { .. })
        ));
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);
        assert!(queue.try_queue(make_migrate_as_source_request()).is_ok());

        // A successful migration stops the instance, which forecloses on future
        // requests to migrate out.
        queue.pop_front();
        queue.notify_instance_state_change(InstanceStateChange::Stopped);
        assert!(queue.try_queue(make_migrate_as_source_request()).is_err());
    }

    #[tokio::test]
    async fn stop_requests_enqueue_after_vm_failure() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::No);
        queue.notify_instance_state_change(InstanceStateChange::Failed);

        assert!(queue.try_queue(ExternalRequest::Stop).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Stop)));
    }

    #[tokio::test]
    async fn reboot_requests_are_idempotent_except_when_stopping() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);
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

    #[tokio::test]
    async fn mutation_requires_not_migrating_out() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::No);

        // Mutating a VM before it has started is allowed.
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_ok());
        assert!(matches!(
            queue.pop_front(),
            Some(ExternalRequest::ReconfigureCrucibleVolume { .. })
        ));

        // Mutating a VM is also allowed while it is starting.
        assert!(queue.try_queue(ExternalRequest::Start).is_ok());
        assert!(matches!(queue.pop_front(), Some(ExternalRequest::Start)));
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_ok());
        assert!(matches!(
            queue.pop_front(),
            Some(ExternalRequest::ReconfigureCrucibleVolume { .. })
        ));

        // And it's allowed once the VM has started running.
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_ok());
        assert!(matches!(
            queue.pop_front(),
            Some(ExternalRequest::ReconfigureCrucibleVolume { .. })
        ));

        // Successfully requesting migration out should block new mutation
        // requests (they should wait for the migration to resolve and then go
        // to the target).
        assert!(queue.try_queue(make_migrate_as_source_request()).is_ok());
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_err());

        // But if the VM resumes (due to a failed migration out) these requests
        // should succeed again.
        assert!(queue.pop_front().is_some());
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_ok());
    }

    #[tokio::test]
    async fn mutation_disallowed_after_stop_requested() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);
        queue.notify_instance_state_change(InstanceStateChange::StartedRunning);

        assert!(queue.try_queue(ExternalRequest::Stop).is_ok());
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_err());

        queue.notify_instance_state_change(InstanceStateChange::Stopped);
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_err());
    }
}
