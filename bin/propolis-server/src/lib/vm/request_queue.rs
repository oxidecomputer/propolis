// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handles requests to change a Propolis server's state or component
//! configuration via the external API.
//!
//! The queue accepts or rejects requests based on a combination of its current
//! state and its knowledge of requests that it has previously queued but that
//! have not yet been processed. The latter knowledge is used to reject requests
//! that will never be fulfilled (because a prior request preempts them) or that
//! may need to be redirected to a migration target.
//!
//! The queue contains no synchronization of its own. Users who want to share a
//! queue between multiple threads must wrap it in a synchronization object.

use std::collections::VecDeque;

use propolis_api_types::instance_spec::SpecKey;
use slog::{info, Logger};
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

/// A request to change a VM's runtime state.
pub(crate) enum StateChangeRequest {
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
}

impl std::fmt::Debug for StateChangeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start => write!(f, "Start"),
            Self::MigrateAsSource { migration_id, .. } => f
                .debug_struct("MigrateAsSource")
                .field("migration_id", migration_id)
                .finish(),
            Self::Reboot => write!(f, "Reboot"),
            Self::Stop => write!(f, "Stop"),
        }
    }
}

/// A request to reconfigure a VM's components.
///
/// NOTE: Successfully queuing a component change request does not guarantee
/// that the request will be processed, because it may be preempted by a VM
/// state change. If this happens the request will fail and notify the
/// submitter using whatever channel is appropriate for the request's type.
pub enum ComponentChangeRequest {
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

impl std::fmt::Debug for ComponentChangeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReconfigureCrucibleVolume { backend_id, .. } => f
                .debug_struct("ReconfigureCrucibleVolume")
                .field("backend_id", backend_id)
                .finish(),
        }
    }
}

/// An external request made of a VM controller via the server API. Handled by
/// the controller's state driver thread.
#[derive(Debug)]
pub(crate) enum ExternalRequest {
    /// A request to change the VM's runtime state.
    State(StateChangeRequest),

    /// A request to reconfigure one of the VM's components.
    Component(ComponentChangeRequest),
}

impl ExternalRequest {
    /// Constructs a VM start request.
    pub fn start() -> Self {
        Self::State(StateChangeRequest::Start)
    }

    /// Constructs a VM stop request.
    pub fn stop() -> Self {
        Self::State(StateChangeRequest::Stop)
    }

    /// Constructs a VM reboot request.
    pub fn reboot() -> Self {
        Self::State(StateChangeRequest::Reboot)
    }

    /// Constructs a request to migrate a VM to another Propolis instance, using
    /// `ws_conn` as the websocket connection to the migration target.
    pub fn migrate_as_source(
        migration_id: Uuid,
        ws_conn: dropshot::WebsocketConnection,
    ) -> Self {
        Self::State(StateChangeRequest::MigrateAsSource {
            migration_id,
            websock: WebsocketConnection(Some(ws_conn)),
        })
    }

    /// Constructs a request to update a Crucible volume's construction request.
    /// The result of this request will be sent to the supplied `result_tx`.
    pub fn reconfigure_crucible_volume(
        backend_id: SpecKey,
        new_vcr_json: String,
        result_tx: super::CrucibleReplaceResultTx,
    ) -> Self {
        Self::Component(ComponentChangeRequest::ReconfigureCrucibleVolume {
            backend_id,
            new_vcr_json,
            result_tx,
        })
    }
}

/// A set of reasons why a request to queue an external state transition can
/// fail.
#[derive(Copy, Clone, Debug, Error)]
pub(crate) enum RequestDeniedReason {
    #[error("Operation requires an active instance")]
    InstanceNotActive,

    #[error("Instance is currently starting")]
    StartInProgress,

    #[error("Instance is already a migration source")]
    AlreadyMigrationSource,

    #[error("Operation cannot be performed on a migration source")]
    InvalidForMigrationSource,

    #[error("Instance is preparing to stop")]
    HaltPending,

    #[error("Instance has migrated out and is being torn down")]
    MigratedOut,

    #[error("Instance has already halted")]
    Halted,

    #[error("Instance failed to start or halted due to a failure")]
    InstanceFailed,
}

/// The possible methods of handling a request to queue a state change.
#[derive(Copy, Clone, Debug)]
enum Disposition {
    /// Put the state change on the queue.
    Enqueue,

    /// Drop the state change silently. This is used to make requests appear
    /// idempotent to callers without making the state driver deal with the
    /// consequences of queuing the same state change request twice.
    Ignore,

    /// Deny the request to change state.
    Deny(RequestDeniedReason),
}

/// A kind of request that can be popped from the queue and then completed.
#[derive(Debug)]
pub(super) enum CompletedRequest {
    Start { succeeded: bool },
    Reboot,
    MigrationOut { succeeded: bool },
    Stop,
}

/// The queue's internal notion of the VM's runtime state.
#[derive(Copy, Clone, Debug)]
enum QueueState {
    /// The instance has not started yet and no one has asked it to start.
    NotStarted,

    /// The instance is not running yet, but the state driver will eventually
    /// try to start it.
    StartPending,

    /// The instance has successfully started.
    Running,

    /// The instance has stopped due to a migration out.
    MigratedOut,

    /// The instance has shut down.
    Stopped,

    /// The instance failed to start.
    Failed,
}

impl QueueState {
    /// If `self` is a state in which new change requests should be denied
    /// unconditionally, returns a `Some` containing an appropriate
    /// [`RequestDeniedReason`]; returns `None` otherwise.
    fn deny_reason(&self) -> Option<RequestDeniedReason> {
        match self {
            Self::MigratedOut => Some(RequestDeniedReason::MigratedOut),
            Self::Stopped => Some(RequestDeniedReason::Halted),
            Self::Failed => Some(RequestDeniedReason::InstanceFailed),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct ExternalRequestQueue {
    /// The queue of unprocessed state change requests.
    state_queue: VecDeque<StateChangeRequest>,

    /// The queue of unprocessed component change requests.
    component_queue: VecDeque<ComponentChangeRequest>,

    /// The "effective" (for purposes of deciding how to dispose of requests)
    /// state of the instance associated with this queue.
    state: QueueState,

    /// True if this queue has enqueued a reboot request that has not been
    /// completed by the state driver.
    awaiting_reboot: bool,

    /// True if this queue has enqueued a stop request that has not been
    /// completed by the state driver.
    awaiting_migration_out: bool,

    /// True if this queue has enqueued a request to migrate out that has not
    /// been completed by the state driver.
    awaiting_stop: bool,

    /// The queue's logger.
    log: Logger,
}

/// Indicates whether this queue's creator will start the relevant instance
/// without waiting for a Start request from the queue.
pub(super) enum InstanceAutoStart {
    Yes,
    No,
}

impl ExternalRequestQueue {
    /// Creates a new queue that logs to the supplied logger.
    pub fn new(log: Logger, auto_start: InstanceAutoStart) -> Self {
        let instance_state = match auto_start {
            InstanceAutoStart::Yes => QueueState::StartPending,
            InstanceAutoStart::No => QueueState::NotStarted,
        };

        Self {
            state_queue: Default::default(),
            component_queue: Default::default(),

            state: instance_state,
            awaiting_reboot: false,
            awaiting_migration_out: false,
            awaiting_stop: false,
            log,
        }
    }

    /// Pops the next request off of the queue. If the queue contains both state
    /// change and component change requests, the next state change request is
    /// popped first (even if it arrived later in time than the next component
    /// change request).
    pub fn pop_front(&mut self) -> Option<ExternalRequest> {
        if let Some(state_change) = self.state_queue.pop_front() {
            Some(ExternalRequest::State(state_change))
        } else {
            self.component_queue.pop_front().map(ExternalRequest::Component)
        }
    }

    /// Indicates whether the queue is empty.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.state_queue.is_empty() && self.component_queue.is_empty()
    }

    /// Attempts to replace the supplied `request` on the queue, returning `Ok`
    /// if the request was accepted and an `Err` otherwise. In the latter case,
    /// the error contains a [`RequestDeniedReason`] that describes why the
    /// request was rejected.
    pub fn try_queue(
        &mut self,
        request: ExternalRequest,
    ) -> Result<(), RequestDeniedReason> {
        // If the queue is in a terminal state, deny the request straightaway
        // (unless it's a stop request, which can be ignored for idempotency).
        let disposition = if let Some(reason) = self.state.deny_reason() {
            if matches!(
                request,
                ExternalRequest::State(StateChangeRequest::Stop)
            ) {
                Disposition::Ignore
            } else {
                Disposition::Deny(reason)
            }
        } else {
            // The instance hasn't stopped yet, so consider this request in
            // light of its current state and the other as-yet unprocessed
            // requests from the queue.
            //
            // In general, try to make state change requests idempotent by
            // ignoring new requests when a request of the appropriate kind is
            // already on the queue, and deny requests to reach a state that is
            // precluded by an earlier state change request.
            match request {
                // Interpret start requests as requests to reach the Running
                // state.
                ExternalRequest::State(StateChangeRequest::Start) => {
                    if self.awaiting_stop {
                        Disposition::Deny(RequestDeniedReason::HaltPending)
                    } else if let QueueState::NotStarted = self.state {
                        Disposition::Enqueue
                    } else {
                        Disposition::Ignore
                    }
                }

                // Only allow one attempt to migrate out at a time (if it works
                // the VM can't migrate out again), and only allow migration out
                // after an instance begins to run.
                ExternalRequest::State(
                    StateChangeRequest::MigrateAsSource { .. },
                ) => {
                    if self.awaiting_migration_out {
                        Disposition::Deny(
                            RequestDeniedReason::AlreadyMigrationSource,
                        )
                    } else if self.awaiting_stop {
                        Disposition::Deny(RequestDeniedReason::HaltPending)
                    } else if matches!(self.state, QueueState::NotStarted) {
                        Disposition::Deny(
                            RequestDeniedReason::InstanceNotActive,
                        )
                    } else {
                        Disposition::Enqueue
                    }
                }

                // Treat reboot requests as a request to take a VM that has
                // already started, reset its state, and resume the VM. If the
                // VM migrates out first, this request needs to be directed to
                // the target, so reject it here to allow the caller to wait for
                // the migration to resolve.
                ExternalRequest::State(StateChangeRequest::Reboot) => {
                    if self.awaiting_migration_out {
                        Disposition::Deny(
                            RequestDeniedReason::InvalidForMigrationSource,
                        )
                    } else if self.awaiting_stop {
                        Disposition::Deny(RequestDeniedReason::HaltPending)
                    } else if matches!(self.state, QueueState::NotStarted) {
                        Disposition::Deny(
                            RequestDeniedReason::InstanceNotActive,
                        )
                    } else if matches!(self.state, QueueState::StartPending) {
                        Disposition::Deny(RequestDeniedReason::StartInProgress)
                    } else if self.awaiting_reboot {
                        Disposition::Ignore
                    } else {
                        Disposition::Enqueue
                    }
                }

                // As with reboots, deny requests to stop a VM that might
                // migrate out first, since the request may need to be directed
                // to the migration target.
                ExternalRequest::State(StateChangeRequest::Stop) => {
                    if self.awaiting_migration_out {
                        Disposition::Deny(
                            RequestDeniedReason::InvalidForMigrationSource,
                        )
                    } else if self.awaiting_stop {
                        Disposition::Ignore
                    } else {
                        Disposition::Enqueue
                    }
                }

                // Always enqueue component change requests, even if the VM has
                // a pending request to stop or migrate out. This allows the
                // state driver to process these requests during a state change,
                // which may be necessary to complete that state change. If the
                // change request is canceled by a later state transition, the
                // queue can use the request data to notify the requestor.
                ExternalRequest::Component(
                    ComponentChangeRequest::ReconfigureCrucibleVolume {
                        ..
                    },
                ) => Disposition::Enqueue,
            }
        };

        info!(&self.log, "Queuing external request";
              "request" => ?request,
              "disposition" => ?disposition);

        match disposition {
            Disposition::Ignore => Ok(()),
            Disposition::Deny(reason) => Err(reason),
            Disposition::Enqueue => {
                match request {
                    ExternalRequest::State(StateChangeRequest::Start) => {
                        assert!(matches!(self.state, QueueState::NotStarted));
                        self.state = QueueState::StartPending;
                    }
                    ExternalRequest::State(
                        StateChangeRequest::MigrateAsSource { .. },
                    ) => {
                        assert!(!self.awaiting_migration_out);
                        self.awaiting_migration_out = true;
                    }
                    ExternalRequest::State(StateChangeRequest::Reboot) => {
                        assert!(!self.awaiting_reboot);
                        self.awaiting_reboot = true;
                    }
                    ExternalRequest::State(StateChangeRequest::Stop) => {
                        assert!(!self.awaiting_stop);
                        self.awaiting_stop = true;
                    }
                    ExternalRequest::Component(_) => {}
                }

                match request {
                    ExternalRequest::State(s) => self.state_queue.push_back(s),
                    ExternalRequest::Component(c) => {
                        self.component_queue.push_back(c)
                    }
                }

                Ok(())
            }
        }
    }

    /// Notifies this queue that the caller has finished processing a
    /// previously-dequeued request, allowing the queue to adjust its
    /// dispositions in response.
    pub(super) fn notify_request_completed(&mut self, req: CompletedRequest) {
        info!(
            &self.log,
            "queue notified of request completion";
            "request" => ?req
        );

        match req {
            CompletedRequest::Start { succeeded } => {
                assert!(matches!(self.state, QueueState::StartPending));
                if succeeded {
                    self.state = QueueState::Running;
                } else {
                    self.state = QueueState::Failed;
                }
            }
            CompletedRequest::Reboot => {
                assert!(matches!(self.state, QueueState::Running));
                assert!(self.awaiting_reboot);
                self.awaiting_reboot = false;
            }
            CompletedRequest::MigrationOut { succeeded } => {
                assert!(matches!(self.state, QueueState::Running));
                assert!(self.awaiting_migration_out);
                self.awaiting_migration_out = false;
                if succeeded {
                    self.state = QueueState::MigratedOut;
                }
            }
            CompletedRequest::Stop => {
                assert!(self.awaiting_stop);
                self.awaiting_stop = false;
                self.state = QueueState::Stopped;
            }
        }
    }

    /// Notifies this queue that the instance has stopped. This routine is meant
    /// to be used in cases where an instance stops for reasons other than an
    /// external request (e.g., a guest-requested chipset-driven shutdown).
    pub(super) fn notify_stopped(&mut self) {
        info!(&self.log, "queue notified that VM has stopped");
        self.state = QueueState::Stopped;
    }
}

// It's possible for an external request queue to be dropped with outstanding
// requests if an event from the guest shuts down the VM before the queue can be
// drained. If this happens, notify anyone waiting on a specific request on the
// queue that the VM is gone.
impl Drop for ExternalRequestQueue {
    fn drop(&mut self) {
        // No special handling is needed for the state change queue:
        //
        // - Requests to start, reboot, and stop are handled asynchronously
        //   (calls to change the instance's state return as soon as they're
        //   queued).
        // - Requests to migrate out contain a connection to the migration
        //   target; dropping this connection tells the target the source is
        //   gone.
        //
        // Drain the component change request queue and send messages to
        // requestors telling them that their requests have been canceled.
        for req in self.component_queue.drain(..) {
            match req {
                // Crucible VCR change requestors wait for their requests to be
                // retired.
                ComponentChangeRequest::ReconfigureCrucibleVolume {
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
        ExternalRequest::State(StateChangeRequest::MigrateAsSource {
            migration_id: Uuid::new_v4(),
            websock: WebsocketConnection(None),
        })
    }

    fn make_reconfigure_crucible_request() -> ExternalRequest {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        ExternalRequest::Component(
            ComponentChangeRequest::ReconfigureCrucibleVolume {
                backend_id: SpecKey::Uuid(Uuid::new_v4()),
                new_vcr_json: "".to_string(),
                result_tx: tx,
            },
        )
    }

    impl ExternalRequest {
        fn assert_start(&self) {
            assert!(
                matches!(self, Self::State(StateChangeRequest::Start)),
                "expected start request, got {self:?}"
            );
        }

        fn assert_stop(&self) {
            assert!(
                matches!(self, Self::State(StateChangeRequest::Stop)),
                "expected stop request, got {self:?}"
            );
        }

        fn assert_reboot(&self) {
            assert!(
                matches!(self, Self::State(StateChangeRequest::Reboot)),
                "expected reboot request, got {self:?}"
            );
        }

        fn assert_migrate_as_source(&self) {
            assert!(
                matches!(
                    self,
                    Self::State(StateChangeRequest::MigrateAsSource { .. })
                ),
                "expected migrate as source request, got {self:?}"
            );
        }
    }

    #[test]
    fn start_requests_become_idempotent_after_first_request() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::No);

        // The first request to start should succeed.
        assert!(queue.try_queue(ExternalRequest::start()).is_ok());

        // The second one should too, but only for idempotency: the queue should
        // then have only one start request on it.
        assert!(queue.try_queue(ExternalRequest::start()).is_ok());
        queue.pop_front().unwrap().assert_start();
        assert!(queue.is_empty());

        // Start requests continue to be ignored even after the instance starts
        // to run.
        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: true,
        });

        assert!(queue.try_queue(ExternalRequest::start()).is_ok());
        assert!(queue.is_empty());
    }

    #[test]
    fn migrate_as_source_is_not_idempotent() {
        // Simulate a running instance.
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);

        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: true,
        });

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
        queue.pop_front().unwrap().assert_migrate_as_source();
        queue.notify_request_completed(CompletedRequest::MigrationOut {
            succeeded: false,
        });

        assert!(queue.try_queue(make_migrate_as_source_request()).is_ok());

        // A successful migration stops the instance, which forecloses on future
        // requests to migrate out.
        queue.pop_front();
        queue.notify_request_completed(CompletedRequest::MigrationOut {
            succeeded: true,
        });

        assert!(queue.try_queue(make_migrate_as_source_request()).is_err());
    }

    #[test]
    fn stop_requests_are_idempotent() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);

        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: true,
        });

        assert!(queue.try_queue(ExternalRequest::stop()).is_ok());
        assert!(queue.try_queue(ExternalRequest::stop()).is_ok());
        queue.pop_front().unwrap().assert_stop();
        assert!(queue.is_empty());
    }

    #[test]
    fn stop_requests_ignored_after_vm_failure() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);

        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: false,
        });

        assert!(queue.try_queue(ExternalRequest::stop()).is_ok());
        assert!(queue.is_empty());
    }

    #[test]
    fn reboot_requests_are_idempotent_except_when_stopping() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);
        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: true,
        });

        // Once the instance is started, reboot requests should be allowed, but
        // after the first, subsequent requests should be dropped for
        // idempotency.
        assert!(queue.is_empty());
        for _ in 0..5 {
            assert!(queue.try_queue(ExternalRequest::reboot()).is_ok());
        }
        queue.pop_front().unwrap().assert_reboot();
        assert!(queue.is_empty());

        // Once the instance has rebooted, new requests can be queued.
        queue.notify_request_completed(CompletedRequest::Reboot);
        assert!(queue.try_queue(ExternalRequest::reboot()).is_ok());
        queue.pop_front().unwrap().assert_reboot();
        queue.notify_request_completed(CompletedRequest::Reboot);

        // If a request to reboot is queued, and then a request to stop is
        // queued, new requests to reboot should always fail, even after the
        // instance finishes rebooting.
        assert!(queue.try_queue(ExternalRequest::reboot()).is_ok());
        assert!(!queue.is_empty());
        assert!(queue.try_queue(ExternalRequest::stop()).is_ok());
        assert!(queue.try_queue(ExternalRequest::reboot()).is_err());
        queue.pop_front().unwrap().assert_reboot();
        queue.notify_request_completed(CompletedRequest::Reboot);
        assert!(queue.try_queue(ExternalRequest::reboot()).is_err());
    }

    #[test]
    fn mutation_disallowed_after_stopped() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);
        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: true,
        });

        assert!(queue.try_queue(ExternalRequest::stop()).is_ok());
        queue.notify_request_completed(CompletedRequest::Stop);
        assert!(queue.try_queue(make_reconfigure_crucible_request()).is_err());
    }

    #[tokio::test]
    async fn vcr_requests_canceled_when_queue_drops() {
        let mut queue =
            ExternalRequestQueue::new(test_logger(), InstanceAutoStart::Yes);

        queue.notify_request_completed(CompletedRequest::Start {
            succeeded: true,
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = ExternalRequest::Component(
            ComponentChangeRequest::ReconfigureCrucibleVolume {
                backend_id: SpecKey::Uuid(Uuid::new_v4()),
                new_vcr_json: "".to_string(),
                result_tx: tx,
            },
        );

        assert!(queue.try_queue(req).is_ok());
        drop(queue);
        let err = rx.await.unwrap().unwrap_err();
        assert_eq!(err.status_code, hyper::StatusCode::GONE);
    }
}
