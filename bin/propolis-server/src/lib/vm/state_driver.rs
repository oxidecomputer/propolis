// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Structures and tasks that handle VM state and configuration change requests.
//!
//! This module handles the second high-level phase of a VM's lifecycle: once a
//! VM's components and services exist, it enters an event loop that changes the
//! VM's state in response to external API requests and signals arriving from
//! within the VM. See the [`ensure`] module for more information about the
//! initialization phase.
//!
//! This module's main struct is the [`StateDriver`], which holds references to
//! an active VM's components, the VM's event queues, and the sender side of a
//! channel that publishes instance state updates. External API requests are
//! routed to the driver's event queue and handled by the driver task. This
//! model ensures that only one task handles VM events and updates VM state; the
//! idea is to minimize the number of different tasks and threads one has to
//! consider when reasoning about concurrency in the VM state machine.
//!
//! On a migration in, the state driver implicitly starts the VM before entering
//! the main event loop:
//!
//! ```text
//!                   +-----------------------+
//!                   | VM components created |
//!                   +-----------+-----------+
//!                               |
//!                               |
//!            Yes +--------------v--------------+ No
//!              +-+ Initialized via migration?  +-+
//!              | +-----------------------------+ |
//!              |                                 |
//!       +------v--------+                        |
//!       | Auto-start VM |                        |
//!       +------+--------+                        |
//!              |                                 |
//! +------------v------------+                    |
//! | Start devices and vCPUs |                    |
//! +------------+------------+                    |
//!              |                                 |
//!              |                                 |
//!              |    +-----------------------+    |
//!              +----> Enter main event loop <----+
//!                   +-----------------------+
//! ```
//!
//! Once in the main event loop, a VM generally remains active until it receives
//! a signal telling it to do something else:
//!
//! ```text
//! +-----------------+   +-----------------+  error during startup
//! | Not yet started |   | Not yet started |       +--------+
//! | (migrating in)  |   |   (Creating)    +-------> Failed |
//! +-------+---------+   +--------+--------+       +--------+
//!         |                      |
//!         |                      | Successful start request
//!         +-----------+          |
//!                    +v----------v-----------+ API/chipset request
//!          +---------+        Running        +------+
//!          |         +---^-------+--------^--+   +--v--------+
//!          |             |       |        +------+ Rebooting |
//!          |             |       |               +-----------+
//! +--------v------+      |       |
//! | Migrating out +------+       | API/chipset request
//! +--------+------+              |
//!          |                +----v-----+
//!          |                | Stopping |
//!          |                +----+-----+
//!          |                     |
//!          |                     |            +-----------------+
//!          |                +----v-----+      |    Destroyed    |
//!          +----------------> Stopped  +------> (after rundown) |
//!                           +----------+      +-----------------+
//! ```
//!
//! The state driver's [`InputQueue`] receives events that can push a running VM
//! out of its steady "running" state. These can come either from the external
//! API or from events happening in the guest (e.g. a vCPU asserting a pin on
//! the virtual chipset that should reset or halt the VM). The policy that
//! determines what API requests can be accepted in which states is implemented
//! in the [`request_queue`] module.
//!
//! The "stopped" and "failed" states are terminal states. When the state driver
//! reaches one of these states, it exits the event loop, returning its final
//! state to the wrapper function that launched the driver. The wrapper task is
//! responsible for running down the VM objects and structures and resetting the
//! server so that it can start another VM.
//!
//! [`ensure`]: crate::vm::ensure

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use dropshot::HttpError;
use propolis_api_types::{
    instance_spec::{components::backends::CrucibleStorageBackend, SpecKey},
    InstanceState, MigrationState,
};
use slog::{error, info};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
    migrate::{
        destination::DestinationProtocol, source::SourceProtocol, MigrateRole,
    },
    spec::StorageBackend,
    vm::{state_publisher::ExternalStateUpdate, BlockBackendMap},
};

use super::{
    ensure::{
        VmEnsureActive, VmEnsureActiveOutput, VmEnsureNotStarted,
        VmEnsureRequest,
    },
    guest_event::{self, GuestEvent},
    objects::VmObjects,
    request_queue::{self, ExternalRequest, InstanceAutoStart},
    state_publisher::{MigrationStateUpdate, StatePublisher},
    InstanceEnsureResponseTx,
};

/// Tells the state driver what to do after handling an event.
#[derive(Debug, PartialEq, Eq)]
enum HandleEventOutcome {
    Continue,
    Exit { final_state: InstanceState },
}

/// A reason for starting a VM.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum VmStartReason {
    MigratedIn,
    ExplicitRequest,
}

/// A kind of event the state driver can handle.
#[derive(Debug)]
enum InputQueueEvent {
    ExternalRequest(ExternalRequest),
    GuestEvent(GuestEvent),
}

/// The lock-guarded parts of a state driver's input queue.
struct InputQueueInner {
    /// State change requests from the external API.
    external_requests: request_queue::ExternalRequestQueue,

    /// State change requests from the external API that were previously read
    /// but not handled immediately.
    buffered_external: VecDeque<ExternalRequest>,

    /// State change requests from the VM's components. These take precedence
    /// over external state change requests.
    guest_events: super::guest_event::GuestEventQueue,
}

impl InputQueueInner {
    fn new(log: slog::Logger, auto_start: InstanceAutoStart) -> Self {
        Self {
            external_requests: request_queue::ExternalRequestQueue::new(
                log, auto_start,
            ),
            guest_events: super::guest_event::GuestEventQueue::default(),
            buffered_external: Default::default(),
        }
    }
}

/// A queue for external state change requests and guest-driven state changes.
pub(super) struct InputQueue {
    /// Contains the input queue's sub-queues, one for external state change
    /// requests and one for events emitted by the VM.
    inner: Mutex<InputQueueInner>,

    /// Notifies the state driver that a new event is present on the queue.
    ///
    /// Notifiers must use [`Notify::notify_one`] when signaling this `Notify`
    /// to guarantee the state driver does not miss incoming messages. See the
    /// comments in [`InputQueue::wait_for_next_event`].
    notify: Notify,
}

impl InputQueue {
    /// Creates a new state driver input queue.
    pub(super) fn new(
        log: slog::Logger,
        auto_start: InstanceAutoStart,
    ) -> Self {
        Self {
            inner: Mutex::new(InputQueueInner::new(log, auto_start)),
            notify: Notify::new(),
        }
    }

    /// Waits for an event to arrive on the input queue and returns it for
    /// processing.
    ///
    /// External requests and guest events are stored in separate queues. If
    /// both queues have events when this routine is called, the guest event
    /// queue takes precedence.
    ///
    /// # Synchronization
    ///
    /// This routine assumes that it is only ever called by one task (the state
    /// driver). If multiple threads call this routine simultaneously, they may
    /// miss wakeups and not return when new events are pushed to the queue or
    /// cause a panic (see below).
    async fn wait_for_next_event(&self) -> InputQueueEvent {
        loop {
            {
                let mut guard = self.inner.lock().unwrap();
                if let Some(guest_event) = guard.guest_events.pop_front() {
                    return InputQueueEvent::GuestEvent(guest_event);
                } else if let Some(req) = guard.buffered_external.pop_front() {
                    return InputQueueEvent::ExternalRequest(req);
                } else if let Some(req) = guard.external_requests.pop_front() {
                    return InputQueueEvent::ExternalRequest(req);
                }
            }

            // It's safe not to use `Notified::enable` here because (1) only one
            // thread (the state driver) can call `wait_for_next_event` on a
            // given input queue, and (2) all the methods of signaling the queue
            // use `notify_one`, which buffers a permit if no one is waiting
            // when the signal arrives. This means that if a notification is
            // sent after the lock is dropped but before `notified()` is called
            // here, the ensuing wait will be satisfied immediately.
            self.notify.notified().await;
        }
    }

    /// Pushes an external request to the end of the buffered external request
    /// list.
    fn buffer_external_request(&self, req: ExternalRequest) {
        let mut guard = self.inner.lock().unwrap();
        guard.buffered_external.push_back(req);
    }

    /// Notifies the external request queue that the instance's state has
    /// changed so that it can change the dispositions for new state change
    /// requests.
    fn notify_instance_state_change(
        &self,
        state: request_queue::InstanceStateChange,
    ) {
        let mut guard = self.inner.lock().unwrap();
        guard.external_requests.notify_instance_state_change(state);
    }

    /// Submits an external state change request to the queue.
    pub(super) fn queue_external_request(
        &self,
        request: ExternalRequest,
    ) -> Result<(), request_queue::RequestDeniedReason> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.external_requests.try_queue(request);
        if result.is_ok() {
            self.notify.notify_one();
        }
        result
    }
}

impl guest_event::VcpuEventHandler for InputQueue {
    fn suspend_halt_event(&self, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard
            .guest_events
            .enqueue(guest_event::GuestEvent::VcpuSuspendHalt(when))
        {
            self.notify.notify_one();
        }
    }

    fn suspend_reset_event(&self, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard
            .guest_events
            .enqueue(guest_event::GuestEvent::VcpuSuspendReset(when))
        {
            self.notify.notify_one();
        }
    }

    fn suspend_triple_fault_event(&self, vcpu_id: i32, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(
            guest_event::GuestEvent::VcpuSuspendTripleFault(vcpu_id, when),
        ) {
            self.notify.notify_one();
        }
    }

    fn unhandled_vm_exit(
        &self,
        vcpu_id: i32,
        exit: propolis::exits::VmExitKind,
    ) {
        panic!("vCPU {}: Unhandled VM exit: {:?}", vcpu_id, exit);
    }

    fn io_error_event(&self, vcpu_id: i32, error: std::io::Error) {
        panic!("vCPU {}: Unhandled vCPU error: {}", vcpu_id, error);
    }
}

impl guest_event::ChipsetEventHandler for InputQueue {
    fn chipset_halt(&self) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(guest_event::GuestEvent::ChipsetHalt) {
            self.notify.notify_one();
        }
    }

    fn chipset_reset(&self) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(guest_event::GuestEvent::ChipsetReset) {
            self.notify.notify_one();
        }
    }
}

/// The context for a VM state driver task's main loop.
struct StateDriver {
    /// The state driver's associated logger.
    log: slog::Logger,

    /// The VM objects this driver is managing.
    objects: Arc<VmObjects>,

    /// The input queue this driver gets events from.
    input_queue: Arc<InputQueue>,

    /// The channel to which this driver publishes external instance state
    /// changes.
    external_state: StatePublisher,

    /// True if the VM is paused.
    paused: bool,

    /// State persisted from previous attempts to migrate out of this VM.
    migration_src_state: crate::migrate::source::PersistentState,
}

/// Contains a state driver's terminal state and the channel it used to publish
/// state updates to the rest of the server. The driver's owner can use these to
/// publish the VM's terminal state after running down all of its objects and
/// services.
pub(super) struct StateDriverOutput {
    /// The channel this driver used to publish external instance state changes.
    pub state_publisher: StatePublisher,

    /// The terminal state of this instance. When the instance completes
    /// rundown, the parent VM publishes this state to the associated channel.
    pub final_state: InstanceState,
}

/// Given an instance ensure request, processes the request and hands the
/// resulting activated VM off to a [`StateDriver`] that will drive the main VM
/// event loop.
///
/// Returns the final state driver disposition. Note that this routine does not
/// return a `Result`; if the VM fails to start, the returned
/// [`StateDriverOutput`] contains appropriate state for a failed VM.
pub(super) async fn ensure_vm_and_launch_driver(
    log: slog::Logger,
    vm: Arc<super::Vm>,
    mut state_publisher: StatePublisher,
    ensure_request: VmEnsureRequest,
    ensure_result_tx: InstanceEnsureResponseTx,
    ensure_options: super::EnsureOptions,
) -> StateDriverOutput {
    let ensure_options = Arc::new(ensure_options);
    let activated_vm = match ensure_active_vm(
        &log,
        &vm,
        &mut state_publisher,
        &ensure_request,
        ensure_result_tx,
        &ensure_options,
    )
    .await
    {
        Ok(activated) => activated,
        Err(e) => {
            error!(log, "failed to activate new VM"; "error" => #%e);
            return StateDriverOutput {
                state_publisher,
                final_state: InstanceState::Failed,
            };
        }
    };

    let VmEnsureActiveOutput { vm_objects, input_queue, vmm_rt_hdl } =
        activated_vm.into_inner();

    let state_driver = StateDriver {
        log,
        objects: vm_objects,
        input_queue,
        external_state: state_publisher,
        paused: false,
        migration_src_state: Default::default(),
    };

    // Run the VM until it exits, then set rundown on the parent VM so that no
    // new external callers can access its objects or services.
    match vmm_rt_hdl
        .spawn(async move {
            let output = state_driver.run(ensure_request.is_migration()).await;
            vm.set_rundown().await;
            output
        })
        .await
    {
        Ok(output) => output,
        Err(e) => panic!("failed to join state driver task: {e}"),
    }
}

/// Processes the supplied `ensure_request` to create a set of VM objects that
/// can be moved into a new `StateDriver`.
async fn ensure_active_vm<'a>(
    log: &'a slog::Logger,
    vm: &'a Arc<super::Vm>,
    state_publisher: &'a mut StatePublisher,
    ensure_request: &'a VmEnsureRequest,
    ensure_result_tx: InstanceEnsureResponseTx,
    ensure_options: &'a Arc<super::EnsureOptions>,
) -> anyhow::Result<VmEnsureActive<'a>> {
    let ensure = VmEnsureNotStarted::new(
        log,
        vm,
        ensure_request,
        ensure_options,
        ensure_result_tx,
        state_publisher,
    );

    if let Some(migrate_request) = ensure_request.migration_info() {
        let migration = match crate::migrate::destination::initiate(
            log,
            migrate_request,
            ensure_options.local_server_addr,
        )
        .await
        {
            Ok(mig) => mig,
            Err(e) => {
                return Err(ensure
                    .fail(e.into())
                    .await
                    .context("creating migration protocol handler"));
            }
        };

        // Delegate the rest of the activation process to the migration
        // protocol. If the migration fails, the callee is responsible for
        // dispatching failure messages to any API clients who are awaiting
        // the results of their instance ensure calls.
        Ok(migration
            .run(ensure)
            .await
            .context("running live migration protocol")?)
    } else {
        let created = ensure
            .create_objects_from_request()
            .await
            .context("creating VM objects for new instance")?;

        Ok(created.ensure_active().await)
    }
}

impl StateDriver {
    /// Directs this state driver to enter its main event loop. The driver may
    /// perform additional tasks (e.g. automatically starting a migration
    /// target) before it begins processing events from its queues.
    pub(super) async fn run(mut self, migrated_in: bool) -> StateDriverOutput {
        info!(self.log, "state driver launched");

        let final_state = if migrated_in {
            match self.start_vm(VmStartReason::MigratedIn).await {
                Ok(()) => self.event_loop().await,
                Err(_) => InstanceState::Failed,
            }
        } else {
            self.event_loop().await
        };

        StateDriverOutput { state_publisher: self.external_state, final_state }
    }

    /// Runs the state driver's main event loop.
    async fn event_loop(&mut self) -> InstanceState {
        info!(self.log, "state driver entered main loop");
        loop {
            let event = self.input_queue.wait_for_next_event().await;
            info!(self.log, "state driver handling event"; "event" => ?event);

            let outcome = match event {
                InputQueueEvent::ExternalRequest(req) => {
                    self.handle_external_request(req).await
                }
                InputQueueEvent::GuestEvent(event) => {
                    self.handle_guest_event(event).await
                }
            };

            info!(self.log, "state driver handled event"; "outcome" => ?outcome);
            match outcome {
                HandleEventOutcome::Continue => {}
                HandleEventOutcome::Exit { final_state } => {
                    info!(self.log, "state driver exiting";
                          "final_state" => ?final_state);

                    return final_state;
                }
            }
        }
    }

    /// Starts the driver's VM by sending start commands to its devices and
    /// vCPUs.
    async fn start_vm(
        &mut self,
        start_reason: VmStartReason,
    ) -> anyhow::Result<()> {
        info!(self.log, "starting instance"; "reason" => ?start_reason);

        // The start sequence is arranged so that calls to block backends can be
        // interleaved with processing of requests from the external request
        // queue. This allows Nexus to reconfigure Crucible backends while they
        // are being activated, which can be necessary if the VM's original
        // specification specifies a Crucible downstairs server that is offline
        // or unavailable. (Downstairs instances can disappear at any time,
        // e.g. due to sled failure, so these configurations aren't necessarily
        // client errors.)
        //
        // Before getting into any of that, handle the synchronous portions of
        // VM startup. First, ensure that the kernel VM and all its associated
        // devices are in the correct initial states.
        let objects = self.objects.lock_shared().await;
        match start_reason {
            // If this VM is a migration target, migration will have properly
            // initialized the vCPUs, but will have left the kernel VM paused.
            // Resume it here before asking any in-kernel components to start.
            VmStartReason::MigratedIn => objects.resume_kernel_vm(),

            // If this VM is starting from scratch, its kernel VM is active, but
            // its vCPUs have not been initialized yet.
            VmStartReason::ExplicitRequest => objects.reset_vcpus(),
        }

        // Send synchronous start commands to all devices.
        for (name, dev) in objects.device_map() {
            info!(self.log, "sending start request to {}", name);
            let res = dev.start();
            if let Err(e) = &res {
                error!(self.log, "startup failed for {}: {:?}", name, e);
                return res;
            }
        }

        // Next, prepare to start block backends. This is done by capturing the
        // current block backend set and creating a future that issues all the
        // start requests.
        //
        // For this to work, the set of block backends to be started must not
        // change while the VM is starting. This is guaranteed because all such
        // requests to hotplug a block backend will be dispatched to the VM's
        // request queue; if any such requests are seen below, they can simply
        // be buffered and handled after the rest of the VM has started.
        async fn start_block_backends(
            log: slog::Logger,
            backends: BlockBackendMap,
        ) -> anyhow::Result<()> {
            for (name, backend) in backends {
                info!(log, "starting block backend {}", name);
                let res = backend.start().await;
                if let Err(e) = &res {
                    error!(
                        log,
                        "startup failed for block backend {}: {:?}", name, e
                    );

                    return res;
                }
            }

            Ok(())
        }

        let block_backends = objects.block_backend_map().clone();
        let block_backend_fut =
            start_block_backends(self.log.clone(), block_backends);
        tokio::pin!(block_backend_fut);

        // Drop the VM object lock before proceeding to allow other API calls
        // that simply read the VM to make progress. Again, note that the set of
        // objects being started still can't change, not because the lock is
        // held, but because the only entity that can change them is the current
        // task, which can decide whether and how to buffer incoming requests.
        drop(objects);

        loop {
            let event = tokio::select! {
                // If the VM successfully starts, return immediately and let
                // the caller process any events that may happen to be on the
                // queue.
                biased;

                res = &mut block_backend_fut => {
                    if res.is_ok() {
                        let objects = &self.objects;
                        objects.lock_exclusive().await.resume_vcpus();
                        self.publish_steady_state(InstanceState::Running);
                        info!(&self.log, "VM successfully started");
                    }

                    return res;
                }

                dequeued = self.input_queue.wait_for_next_event() => {
                    dequeued
                }
            };

            // The VM's vCPUs haven't been started yet, so there should be no
            // way for the VM to produce any guest events to handle.
            let InputQueueEvent::ExternalRequest(req) = event else {
                unreachable!("can't get guest events before the VM starts");
            };

            // Handle requests to reconfigure one of the existing Crucible
            // volumes inline, but buffer other requests so that they can be
            // handled after the VM has finished starting.
            //
            // Buffering some requests and servicing others can theoretically
            // change the order in which those requests are retired. That's not
            // a problem here because the request queue will stop accepting new
            // VCR change requests once a request to stop has been queued.
            match req {
                ExternalRequest::Stop => {
                    info!(
                        &self.log,
                        "got request to stop while still starting"
                    );

                    // Buffer this request to stop so that it can be processed
                    // as part of the main event loop once the VM finishes
                    // starting.
                    //
                    // It is possible, at least in theory, to drop the block
                    // backend startup future (or cooperatively cancel it) and
                    // then pause/halt the VM immediately. This requires block
                    // backends' startup operations to be cancel-safe (in the
                    // sense that it must be possible to pause and halt a
                    // backend whose startup sequence was interrupted). Since
                    // block backends aren't currently required to guarantee
                    // this, just buffer the stop request.
                    self.input_queue.buffer_external_request(req);
                }
                ExternalRequest::ReconfigureCrucibleVolume {
                    backend_id,
                    new_vcr_json,
                    result_tx,
                } => {
                    let _ = result_tx.send(
                        self.reconfigure_crucible_volume(
                            &backend_id,
                            new_vcr_json,
                        )
                        .await,
                    );
                }
                // The request queue is expected to reject (or at least silently
                // ignore) requests to migrate or reboot an instance that hasn't
                // reported that it's fully started. Similarly, requests to
                // start a VM that's already starting are expected to be ignored
                // for idempotency.
                r @ ExternalRequest::Start
                | r @ ExternalRequest::MigrateAsSource { .. }
                | r @ ExternalRequest::Reboot => {
                    unreachable!(
                        "external request {r:?} shouldn't be queued while \
                        starting"
                    );
                }
            }
        }
    }

    async fn handle_guest_event(
        &mut self,
        event: GuestEvent,
    ) -> HandleEventOutcome {
        match event {
            GuestEvent::VcpuSuspendHalt(_when) => {
                info!(self.log, "Halting due to VM suspend event",);
                self.do_halt().await;
                HandleEventOutcome::Exit {
                    final_state: InstanceState::Destroyed,
                }
            }
            GuestEvent::VcpuSuspendReset(_when) => {
                info!(self.log, "Resetting due to VM suspend event");
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
            GuestEvent::VcpuSuspendTripleFault(vcpu_id, _when) => {
                info!(
                    self.log,
                    "Resetting due to triple fault on vCPU {}", vcpu_id
                );
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
            GuestEvent::ChipsetHalt => {
                info!(self.log, "Halting due to chipset-driven halt");
                self.do_halt().await;
                HandleEventOutcome::Exit {
                    final_state: InstanceState::Destroyed,
                }
            }
            GuestEvent::ChipsetReset => {
                info!(self.log, "Resetting due to chipset-driven reset");
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
        }
    }

    async fn handle_external_request(
        &mut self,
        request: ExternalRequest,
    ) -> HandleEventOutcome {
        match request {
            ExternalRequest::Start => {
                match self.start_vm(VmStartReason::ExplicitRequest).await {
                    Ok(()) => HandleEventOutcome::Continue,
                    Err(_) => HandleEventOutcome::Exit {
                        final_state: InstanceState::Failed,
                    },
                }
            }
            ExternalRequest::MigrateAsSource { migration_id, websock } => {
                self.migrate_as_source(migration_id, websock.into_inner())
                    .await;

                // The callee either queues its own stop request (on a
                // successful migration out) or resumes the VM (on a failed
                // migration out). Either way, the main loop can just proceed to
                // process the queue as normal.
                HandleEventOutcome::Continue
            }
            ExternalRequest::Reboot => {
                self.do_reboot().await;
                HandleEventOutcome::Continue
            }
            ExternalRequest::Stop => {
                self.do_halt().await;
                HandleEventOutcome::Exit {
                    final_state: InstanceState::Destroyed,
                }
            }
            ExternalRequest::ReconfigureCrucibleVolume {
                backend_id,
                new_vcr_json,
                result_tx,
            } => {
                let _ = result_tx.send(
                    self.reconfigure_crucible_volume(&backend_id, new_vcr_json)
                        .await,
                );
                HandleEventOutcome::Continue
            }
        }
    }

    async fn do_reboot(&mut self) {
        info!(self.log, "resetting instance");

        self.external_state
            .update(ExternalStateUpdate::Instance(InstanceState::Rebooting));

        self.objects.lock_exclusive().await.reboot().await;

        // Notify other consumers that the instance successfully rebooted and is
        // now back to Running.
        self.input_queue.notify_instance_state_change(
            request_queue::InstanceStateChange::Rebooted,
        );
        self.external_state
            .update(ExternalStateUpdate::Instance(InstanceState::Running));
    }

    async fn do_halt(&mut self) {
        info!(self.log, "stopping instance");
        self.external_state
            .update(ExternalStateUpdate::Instance(InstanceState::Stopping));

        {
            let mut guard = self.objects.lock_exclusive().await;

            // Entities expect to be paused before being halted. Note that the VM
            // may be paused already if it is being torn down after a successful
            // migration out.
            if !self.paused {
                guard.pause().await;
                self.paused = true;
            }

            guard.halt().await;
        }

        self.publish_steady_state(InstanceState::Stopped);
    }

    fn publish_steady_state(&mut self, state: InstanceState) {
        let change = match state {
            InstanceState::Running => {
                request_queue::InstanceStateChange::StartedRunning
            }
            InstanceState::Stopped => {
                request_queue::InstanceStateChange::Stopped
            }
            InstanceState::Failed => request_queue::InstanceStateChange::Failed,
            _ => panic!(
                "Called publish_steady_state on non-terminal state {:?}",
                state
            ),
        };

        self.input_queue.notify_instance_state_change(change);
        self.external_state.update(ExternalStateUpdate::Instance(state));
    }

    async fn migrate_as_source(
        &mut self,
        migration_id: Uuid,
        websock: dropshot::WebsocketConnection,
    ) {
        let conn = tokio_tungstenite::WebSocketStream::from_raw_socket(
            websock.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;

        let migration = match crate::migrate::source::initiate(
            &self.log,
            migration_id,
            conn,
            &self.objects,
            &self.migration_src_state,
        )
        .await
        {
            Ok(migration) => migration,
            Err(_) => {
                self.external_state.update(ExternalStateUpdate::Migration(
                    MigrationStateUpdate {
                        id: migration_id,
                        state: MigrationState::Error,
                        role: MigrateRole::Source,
                    },
                ));

                return;
            }
        };

        // Publish that migration is in progress before actually launching the
        // migration task.
        self.external_state.update(ExternalStateUpdate::Complete(
            InstanceState::Migrating,
            MigrationStateUpdate {
                state: MigrationState::Sync,
                id: migration_id,
                role: MigrateRole::Source,
            },
        ));

        match migration
            .run(
                &self.objects,
                &mut self.external_state,
                &mut self.migration_src_state,
            )
            .await
        {
            Ok(()) => {
                info!(self.log, "migration out succeeded, queuing stop");
                // On a successful migration out, the protocol promises to leave
                // the VM objects in a paused state, so don't pause them again.
                self.paused = true;
                self.input_queue
                    .queue_external_request(ExternalRequest::Stop)
                    .expect("can always queue a request to stop");
            }
            Err(e) => {
                info!(self.log, "migration out failed, resuming";
                      "error" => ?e);

                self.publish_steady_state(InstanceState::Running);
            }
        }
    }

    async fn reconfigure_crucible_volume(
        &self,
        backend_id: &SpecKey,
        new_vcr_json: String,
    ) -> super::CrucibleReplaceResult {
        info!(self.log, "request to replace Crucible VCR";
              "backend_id" => %backend_id);

        let mut objects = self.objects.lock_exclusive().await;
        let backend = objects
            .crucible_backends()
            .get(backend_id)
            .ok_or_else(|| {
                let msg = format!("No crucible backend for id {backend_id}");
                dropshot::HttpError::for_not_found(Some(msg.clone()), msg)
            })?
            .clone();

        let Some(disk) = objects.instance_spec_mut().disks.iter_mut().find(
            |(_id, device)| device.device_spec.backend_id() == backend_id,
        ) else {
            let msg = format!("no disk in spec with backend ID {backend_id}");
            return Err(HttpError::for_not_found(Some(msg.clone()), msg));
        };

        let StorageBackend::Crucible(CrucibleStorageBackend {
            request_json: old_vcr_json,
            readonly,
        }) = &disk.1.backend_spec
        else {
            let msg = format!(
                "disk {} has backend {backend_id} but its kind is {}",
                disk.0,
                disk.1.backend_spec.kind()
            );
            return Err(HttpError::for_not_found(Some(msg.clone()), msg));
        };

        let replace_result = backend
            .vcr_replace(old_vcr_json.as_str(), &new_vcr_json)
            .await
            .map_err(|e| {
                dropshot::HttpError::for_bad_request(
                    Some(e.to_string()),
                    e.to_string(),
                )
            })?;

        disk.1.backend_spec =
            StorageBackend::Crucible(CrucibleStorageBackend {
                readonly: *readonly,
                request_json: new_vcr_json,
            });

        info!(self.log, "replaced Crucible VCR"; "backend_id" => %backend_id);

        Ok(replace_result)
    }
}
