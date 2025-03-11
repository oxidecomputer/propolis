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
            Self::MigrateAsSource { migration_id, websock: _ } => f
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
/// state change. If this happens, the request will fail and notify the
/// submitter using whatever channel is appropriate for the request's type.
pub enum ComponentChangeRequest {
    /// Attempts to update the volume construction request for the supplied
    /// Crucible volume.
    ///
    /// TODO: Due to https://github.com/oxidecomputer/crucible/issues/871, this
    /// is only allowed once the VM is started and the volume has activated, but
    /// it should be allowed even before the VM has started.
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
    pub const fn start() -> Self {
        Self::State(StateChangeRequest::Start)
    }

    /// Constructs a VM stop request.
    pub const fn stop() -> Self {
        Self::State(StateChangeRequest::Stop)
    }

    /// Constructs a VM reboot request.
    pub const fn reboot() -> Self {
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

    fn is_stop(&self) -> bool {
        matches!(self, Self::State(StateChangeRequest::Stop))
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

/// A kind of request that can be popped from the queue and then completed.
#[derive(Copy, Clone, Debug)]
pub(super) enum CompletedRequest {
    Start { succeeded: bool },
    Reboot,
    MigrationOut { succeeded: bool },
    Stop,
}

/// The queue's internal notion of the VM's runtime state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum QueueState {
    /// The instance has not started yet and no one has asked it to start.
    NotStarted,

    /// The instance is not running yet, but it's on its way there: either the
    /// state driver is actively trying to start it, or there is a request that,
    /// when processed, will direct the driver to start the instance.
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

    /// True if this queue has enqueued a request to migrate out that has not
    /// been completed by the state driver.
    awaiting_migration_out: bool,

    /// True if this queue has enqueued a stop request that has not been
    /// completed by the state driver.
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
        let should_queue = self.should_queue(&request);
        match should_queue {
            Ok(true) => {
                info!(
                    &self.log,
                    "enqueued external request";
                    "request" => ?request
                );
            }
            Ok(false) => {
                info!(
                    &self.log,
                    "ignored external request";
                    "request" => ?request
                );

                return Ok(());
            }
            Err(reason) => {
                info!(
                    &self.log,
                    "denied external request";
                    "request" => ?request,
                    "reason" => %reason
                );

                return Err(reason);
            }
        }

        match request {
            ExternalRequest::State(StateChangeRequest::Start) => {
                assert_eq!(self.state, QueueState::NotStarted);
                self.state = QueueState::StartPending;
            }
            ExternalRequest::State(StateChangeRequest::MigrateAsSource {
                ..
            }) => {
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
            ExternalRequest::Component(c) => self.component_queue.push_back(c),
        }

        Ok(())
    }

    /// Determines whether the supplied `request` should be queued, returning:
    ///
    /// - `Ok(true)` if the request was enqueued,
    /// - `Ok(false)` if the request was ignored, and
    /// - `Err(reason)` if the request was denied.
    fn should_queue(
        &mut self,
        request: &ExternalRequest,
    ) -> Result<bool, RequestDeniedReason> {
        // If the queue is in a terminal state, deny the request straightaway
        // (unless it's a stop request, which can be ignored for idempotency).
        if let Some(reason) = self.state.deny_reason() {
            if request.is_stop() {
                return Ok(false);
            } else {
                return Err(reason);
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
                        return Err(RequestDeniedReason::HaltPending);
                    } else if self.state != QueueState::NotStarted {
                        return Ok(false);
                    }
                }

                // Only allow one attempt to migrate out at a time (if it works
                // the VM can't migrate out again), and only allow migration out
                // after an instance begins to run.
                ExternalRequest::State(
                    StateChangeRequest::MigrateAsSource { .. },
                ) => {
                    if self.awaiting_migration_out {
                        return Err(
                            RequestDeniedReason::AlreadyMigrationSource,
                        );
                    } else if self.awaiting_stop {
                        return Err(RequestDeniedReason::HaltPending);
                    } else if self.state == QueueState::NotStarted {
                        return Err(RequestDeniedReason::InstanceNotActive);
                    }
                }

                // Treat reboot requests as a request to take a VM that has
                // already started, reset its state, and resume the VM. If the
                // VM migrates out first, this request needs to be directed to
                // the target, so reject it here to allow the caller to wait for
                // the migration to resolve.
                ExternalRequest::State(StateChangeRequest::Reboot) => {
                    if self.awaiting_migration_out {
                        return Err(
                            RequestDeniedReason::InvalidForMigrationSource,
                        );
                    } else if self.awaiting_stop {
                        return Err(RequestDeniedReason::HaltPending);
                    } else if self.state == QueueState::NotStarted {
                        return Err(RequestDeniedReason::InstanceNotActive);
                    } else if self.state == QueueState::StartPending {
                        return Err(RequestDeniedReason::StartInProgress);
                    } else if self.awaiting_reboot {
                        return Ok(false);
                    }
                }

                // As with reboots, deny requests to stop a VM that might
                // migrate out first, since the request may need to be directed
                // to the migration target.
                ExternalRequest::State(StateChangeRequest::Stop) => {
                    if self.awaiting_migration_out {
                        return Err(
                            RequestDeniedReason::InvalidForMigrationSource,
                        );
                    } else if self.awaiting_stop {
                        return Ok(false);
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
                ) => {}
            }
        };

        Ok(true)
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
                assert_eq!(self.state, QueueState::StartPending);
                if succeeded {
                    self.state = QueueState::Running;
                } else {
                    self.state = QueueState::Failed;
                }
            }
            CompletedRequest::Reboot => {
                assert_eq!(self.state, QueueState::Running);
                assert!(self.awaiting_reboot);
                self.awaiting_reboot = false;
            }
            CompletedRequest::MigrationOut { succeeded } => {
                assert_eq!(self.state, QueueState::Running);
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

    use proptest::prelude::*;
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
        #[track_caller]
        fn assert_start(&self) {
            assert!(
                matches!(self, Self::State(StateChangeRequest::Start)),
                "expected start request, got {self:?}"
            );
        }

        #[track_caller]
        fn assert_stop(&self) {
            assert!(self.is_stop(), "expected stop request, got {self:?}");
        }

        #[track_caller]
        fn assert_reboot(&self) {
            assert!(
                matches!(self, Self::State(StateChangeRequest::Reboot)),
                "expected reboot request, got {self:?}"
            );
        }

        #[track_caller]
        fn assert_migrate_as_source(&self) {
            assert!(
                matches!(
                    self,
                    Self::State(StateChangeRequest::MigrateAsSource { .. })
                ),
                "expected migrate as source request, got {self:?}"
            );
        }

        #[track_caller]
        fn assert_reconfigure_crucible(&self) {
            assert!(
                matches!(
                    self,
                    Self::Component(
                        ComponentChangeRequest::ReconfigureCrucibleVolume { .. }
                    )
                ),
                "expected Crucible reconfiguration request, got {self:?}"
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

    /// A helper for generating requests as part of a property testing strategy.
    /// `proptest` requires values that are the output of a `Strategy` to be
    /// `Clone`, which `ExternalRequest` is not. To get around this, create a
    /// strategy that returns variants of this enum and have a `From` impl that
    /// then creates requests of the appropriate kind.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RequestKind {
        Start { will_succeed: bool },
        Stop,
        Reboot,
        Migrate { will_succeed: bool },
        ReconfigureCrucible,
    }

    impl From<RequestKind> for ExternalRequest {
        fn from(value: RequestKind) -> Self {
            match value {
                RequestKind::Start { will_succeed: _ } => {
                    ExternalRequest::start()
                }
                RequestKind::Stop => ExternalRequest::stop(),
                RequestKind::Reboot => ExternalRequest::reboot(),
                RequestKind::Migrate { will_succeed: _ } => {
                    make_migrate_as_source_request()
                }
                RequestKind::ReconfigureCrucible => {
                    make_reconfigure_crucible_request()
                }
            }
        }
    }

    fn request_strategy() -> impl Strategy<Value = RequestKind> {
        prop_oneof![
            Just(RequestKind::Start { will_succeed: true }),
            Just(RequestKind::Start { will_succeed: false }),
            Just(RequestKind::Stop),
            Just(RequestKind::Reboot),
            Just(RequestKind::Migrate { will_succeed: true }),
            Just(RequestKind::Migrate { will_succeed: false }),
            Just(RequestKind::ReconfigureCrucible),
        ]
    }

    proptest! {
        // Tests the behavior of the request queue in circumstances where start
        // requests are queued, but never actually acknowledged.
        #[test]
        fn request_queuing_before_start_acknowledged(
            reqs in prop::collection::vec(request_strategy(), 0..100)
        ) {
            let mut queue =
                ExternalRequestQueue::new(test_logger(), InstanceAutoStart::No);

            let mut started = false;
            let mut stop_requested = false;
            let mut migrating_out = false;
            for req in reqs {
                let result = queue.try_queue(req.into());
                match req {
                    RequestKind::Start { .. } => {
                        if !stop_requested {
                            assert!(result.is_ok());
                            started = true;
                        } else {
                            assert!(result.is_err());
                        }
                    }

                    RequestKind::Stop => {
                        if !migrating_out {
                            assert!(result.is_ok());
                            stop_requested = true;
                        } else {
                            assert!(result.is_err());
                        }
                    }

                    RequestKind::Reboot => {
                        assert!(result.is_err());
                    }

                    RequestKind::Migrate { .. } => {
                        if started && !stop_requested && !migrating_out {
                            assert!(result.is_ok());
                            migrating_out = true;
                        } else {
                            assert!(result.is_err());
                        }
                    }

                    RequestKind::ReconfigureCrucible => {
                        assert!(result.is_ok());
                    }
                }
            }
        }

        // Tests the behavior of the request queue in circumstances where every
        // request made of the state driver completes immediately.
        #[test]
        fn request_queuing_with_immediate_dequeueing(
            reqs in prop::collection::vec(request_strategy(), 0..100)
        ) {
            let mut queue =
                ExternalRequestQueue::new(test_logger(), InstanceAutoStart::No);

            // True once a start request has been queued.
            let mut start_requested = false;

            // True once the VM reaches a terminal state (stopped, failed,
            // migrated out).
            let mut halted = false;
            for req in reqs {
                let result = queue.try_queue(req.into());
                match req {
                    // Start requests always succeed (though they may be
                    // ignored and not queued) on a non-halted VM.
                    RequestKind::Start { will_succeed } => {
                        if !halted {
                            assert!(result.is_ok());

                            // This request is only enqueued if it is the first
                            // request to start.
                            if !start_requested {
                                start_requested = true;
                                queue.pop_front().unwrap().assert_start();
                                let completed = CompletedRequest::Start {
                                    succeeded: will_succeed
                                };

                                queue.notify_request_completed(completed);

                                // Telling the queue that a start attempt failed
                                // moves the queue to a terminal state.
                                if !will_succeed {
                                    halted = true;
                                }
                            } else {
                                assert!(queue.is_empty());
                            }
                        } else {
                            assert!(result.is_err());
                            assert!(queue.is_empty());
                        }
                    }

                    // Stop requests always succeed (they are never denied), but
                    // they are ignored for VMs that have already halted.
                    RequestKind::Stop => {
                        assert!(result.is_ok());
                        if !halted {
                            queue.pop_front().unwrap().assert_stop();
                            queue.notify_request_completed(
                                CompletedRequest::Stop
                            );

                            halted = true;
                        } else {
                            assert!(queue.is_empty());
                        }
                    }

                    // Reboot requests are always enqueued if the VM is active.
                    // They are ignored if there's a pending migration out, but
                    // in this test there is never a *pending* migration out,
                    // since all requests are dequeued and processed
                    // immediately.
                    RequestKind::Reboot => {
                        if start_requested && !halted {
                            assert!(result.is_ok());
                            queue.pop_front().unwrap().assert_reboot();
                            queue.notify_request_completed(
                                CompletedRequest::Reboot
                            );
                        } else {
                            assert!(result.is_err());
                            assert!(queue.is_empty());
                        }
                    }

                    // Migration requests have the same disposition as reboot
                    // requests.
                    RequestKind::Migrate { will_succeed } => {
                        if start_requested && !halted {
                            assert!(result.is_ok());
                            queue
                                .pop_front()
                                .unwrap()
                                .assert_migrate_as_source();


                            let completed = CompletedRequest::MigrationOut {
                                succeeded: will_succeed
                            };

                            queue.notify_request_completed(completed);
                            if will_succeed {
                                halted = true;
                            }
                        } else {
                            assert!(result.is_err());
                            assert!(queue.is_empty());
                        }
                    }

                    // Crucible reconfiguration requests are always queued for
                    // unhalted VMs.
                    RequestKind::ReconfigureCrucible => {
                        if !halted {
                            assert!(result.is_ok());
                            queue
                                .pop_front()
                                .unwrap()
                                .assert_reconfigure_crucible();
                        } else {
                            assert!(result.is_err());
                            assert!(queue.is_empty());
                        }
                    }
                }
            }
        }
    }

    /// An operation that can be performed during a [`QueueDequeueTest`].
    #[derive(Clone, Copy, Debug)]
    enum QueueOp {
        Enqueue(RequestKind),
        Dequeue,
    }

    fn queue_op_strategy() -> impl Strategy<Value = QueueOp> {
        prop_oneof![
            request_strategy().prop_map(QueueOp::Enqueue),
            Just(QueueOp::Dequeue)
        ]
    }

    /// A helper that queues and dequeues requests in a proptest-generated
    /// order and that sends fake completion notifications back to the request
    /// queue.
    struct QueueDequeueTest {
        /// The external request queue under test.
        queue: ExternalRequestQueue,

        /// The set of state change requests that the helper expects to see from
        /// the external queue.
        expected_state: VecDeque<RequestKind>,

        /// The set of component change requests that the helper expects to see
        /// from the external queue.
        expected_component: VecDeque<RequestKind>,

        /// True if the helper has queued a request to start its fake VM.
        start_requested: bool,

        /// True if the helper has successfully started its fake VM.
        started: bool,

        /// True if the helper has queued a request to stop its fake VM.
        stop_requested: bool,

        /// True if the helper has an outstanding request to reboot its fake VM.
        reboot_requested: bool,

        /// True if the helper has an outstanding request to migrate its fake
        /// VM.
        migrate_out_requested: bool,

        /// True if the fake VM is halted (for any reason).
        halted: bool,
    }

    impl QueueDequeueTest {
        fn new() -> Self {
            Self {
                queue: ExternalRequestQueue::new(
                    test_logger(),
                    InstanceAutoStart::No,
                ),
                expected_state: Default::default(),
                expected_component: Default::default(),
                start_requested: false,
                started: false,
                stop_requested: false,
                reboot_requested: false,
                migrate_out_requested: false,
                halted: false,
            }
        }

        fn run(&mut self, ops: Vec<QueueOp>) {
            for op in ops {
                match op {
                    QueueOp::Enqueue(request) => self.queue_request(request),
                    QueueOp::Dequeue => {
                        self.dequeue_request();
                        if self.halted {
                            return;
                        }
                    }
                }
            }
        }

        /// Submits the supplied `request` to the external request queue,
        /// determines the expected result of that submission based on the
        /// helper's current flags, and asserts that the result matches the
        /// helper's expectation. If the helper expects the request to be
        /// queued, it pushes an entry to its internal expected-change queues.
        fn queue_request(&mut self, request: RequestKind) {
            let result = self.queue.try_queue(request.into());
            match request {
                RequestKind::Start { .. } => {
                    if self.halted || self.stop_requested {
                        assert!(result.is_err());
                        return;
                    }

                    assert!(result.is_ok());
                    if !self.start_requested {
                        self.start_requested = true;
                        self.expected_state.push_back(request);
                    }
                }
                RequestKind::Stop => {
                    if self.halted || self.stop_requested {
                        assert!(result.is_ok());
                        return;
                    }

                    if self.migrate_out_requested {
                        assert!(result.is_err());
                        return;
                    }

                    self.stop_requested = true;
                    self.expected_state.push_back(request);
                }
                RequestKind::Reboot => {
                    if !self.started
                        || self.halted
                        || self.stop_requested
                        || self.migrate_out_requested
                    {
                        assert!(result.is_err());
                        return;
                    }

                    assert!(result.is_ok());
                    if !self.reboot_requested {
                        self.reboot_requested = true;
                        self.expected_state.push_back(request);
                    }
                }
                RequestKind::Migrate { .. } => {
                    if (!self.started && !self.start_requested)
                        || self.halted
                        || self.stop_requested
                        || self.migrate_out_requested
                    {
                        assert!(result.is_err());
                        return;
                    }

                    assert!(result.is_ok());
                    self.expected_state.push_back(request);
                    self.migrate_out_requested = true;
                }
                RequestKind::ReconfigureCrucible => {
                    if self.halted {
                        assert!(result.is_err());
                        return;
                    }

                    assert!(result.is_ok());
                    self.expected_component.push_back(request);
                }
            }
        }

        /// Pops a request from the helper's external queue and verifies that it
        /// matches the first request on the helper's expected-change queue. If
        /// the requests do match, sends a completion notification to the
        /// external queue.
        fn dequeue_request(&mut self) {
            let (dequeued, expected) = match (
                self.queue.pop_front(),
                self.expected_state
                    .pop_front()
                    .or_else(|| self.expected_component.pop_front()),
            ) {
                (None, None) => return,
                (Some(d), None) => {
                    panic!("dequeued request {d:?} but expected nothing")
                }
                (None, Some(e)) => {
                    panic!("expected request {e:?} but dequeued nothing")
                }
                (Some(d), Some(e)) => (d, e),
            };

            match (dequeued, expected) {
                (
                    ExternalRequest::State(StateChangeRequest::Start),
                    RequestKind::Start { will_succeed },
                ) => {
                    self.queue.notify_request_completed(
                        CompletedRequest::Start { succeeded: will_succeed },
                    );
                    if will_succeed {
                        self.started = true;
                    } else {
                        self.halted = true;
                    }
                }
                (
                    ExternalRequest::State(StateChangeRequest::Stop),
                    RequestKind::Stop,
                ) => {
                    self.queue.notify_request_completed(CompletedRequest::Stop);
                    self.halted = true;
                }
                (
                    ExternalRequest::State(StateChangeRequest::Reboot),
                    RequestKind::Reboot,
                ) => {
                    self.queue
                        .notify_request_completed(CompletedRequest::Reboot);
                    self.reboot_requested = false;
                }
                (
                    ExternalRequest::State(
                        StateChangeRequest::MigrateAsSource { .. },
                    ),
                    RequestKind::Migrate { will_succeed },
                ) => {
                    self.queue.notify_request_completed(
                        CompletedRequest::MigrationOut {
                            succeeded: will_succeed,
                        },
                    );
                    self.migrate_out_requested = false;
                    if will_succeed {
                        self.halted = true;
                    }
                }
                (
                    ExternalRequest::Component(
                        ComponentChangeRequest::ReconfigureCrucibleVolume {
                            ..
                        },
                    ),
                    RequestKind::ReconfigureCrucible,
                ) => {}
                (d, e) => panic!(
                    "dequeued request {d:?} but expected to dequeue {e:?}\n\
                    remaining queue: {:#?}\n\
                    remaining expected (state): {:#?}\n\
                    remaining expected (components): {:#?}",
                    self.queue, self.expected_state, self.expected_component
                ),
            }
        }
    }

    proptest! {
        #[test]
        fn request_queue_dequeue(
            ops in prop::collection::vec(queue_op_strategy(), 0..100)
        ) {
            let mut test = QueueDequeueTest::new();
            test.run(ops);
        }
    }
}
