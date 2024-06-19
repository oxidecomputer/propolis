// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! It drives the state vroom vroom

use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use propolis_server_config::Config;

use super::guest_event;

struct InputQueueInner {
    external_requests: super::request_queue::ExternalRequestQueue,
    guest_events: super::guest_event::GuestEventQueue,
}

impl InputQueueInner {
    fn new(log: slog::Logger) -> Self {
        Self {
            external_requests: super::request_queue::ExternalRequestQueue::new(
                log,
            ),
            guest_events: super::guest_event::GuestEventQueue::default(),
        }
    }
}

pub(super) struct InputQueue {
    inner: Mutex<InputQueueInner>,
    cv: Condvar,
}

impl InputQueue {
    pub(super) fn new(log: slog::Logger) -> Self {
        Self {
            inner: Mutex::new(InputQueueInner::new(log)),
            cv: Condvar::new(),
        }
    }
}

impl guest_event::GuestEventHandler for InputQueue {
    fn suspend_halt_event(&self, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard
            .guest_events
            .enqueue(guest_event::GuestEvent::VcpuSuspendHalt(when))
        {
            self.cv.notify_all();
        }
    }

    fn suspend_reset_event(&self, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard
            .guest_events
            .enqueue(guest_event::GuestEvent::VcpuSuspendReset(when))
        {
            self.cv.notify_all();
        }
    }

    fn suspend_triple_fault_event(&self, vcpu_id: i32, when: Duration) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(
            guest_event::GuestEvent::VcpuSuspendTripleFault(vcpu_id, when),
        ) {
            self.cv.notify_all();
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
            self.cv.notify_all();
        }
    }

    fn chipset_reset(&self) {
        let mut guard = self.inner.lock().unwrap();
        if guard.guest_events.enqueue(guest_event::GuestEvent::ChipsetReset) {
            self.cv.notify_all();
        }
    }
}

/// The context for a VM state driver task.
pub(super) struct StateDriver {
    log: slog::Logger,
    parent_vm: Arc<super::Vm>,
    input_queue: Arc<InputQueue>,
    external_state_tx: super::InstanceStateTx,
    state_gen: u64,
    paused: bool,
}

impl StateDriver {
    pub(super) fn new(
        log: slog::Logger,
        vm: Arc<super::Vm>,
        input_queue: Arc<InputQueue>,
        external_state_tx: super::InstanceStateTx,
    ) -> Self {
        let log = log.new(slog::o!("component" => "state_driver"));
        Self {
            log,
            parent_vm: vm,
            input_queue,
            external_state_tx,
            state_gen: 0,
            paused: false,
        }
    }

    pub(super) async fn run(
        self,
        ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
        external_state_rx: super::InstanceStateRx,
    ) {
        if self.initialize_vm(ensure_request, external_state_rx).is_err() {
            self.parent_vm.start_failed();
            return;
        }
    }

    fn initialize_vm(
        &self,
        ensure_request: propolis_api_types::InstanceSpecEnsureRequest,
        external_state_rx: super::InstanceStateRx,
    ) -> anyhow::Result<()> {
        todo!("gjc");
    }
}
