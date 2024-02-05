// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tasks for vCPU backing threads and controls for them.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use propolis::{
    bhyve_api,
    exits::{self, SuspendDetail, VmExitKind},
    vcpu::Vcpu,
    VmEntry,
};
use slog::{debug, error, info};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VcpuTaskError {
    #[error("Failed to spawn a vCPU backing thread: {0}")]
    BackingThreadSpawnFailed(std::io::Error),
}

pub struct VcpuTasks {
    tasks: Vec<(propolis::tasks::TaskCtrl, std::thread::JoinHandle<()>)>,
    generation: Arc<AtomicUsize>,
}

pub trait VcpuEventHandler: Send + Sync {
    fn suspend_halt_event(&self, vcpu_id: i32);
    fn suspend_reset_event(&self, vcpu_id: i32);
    fn suspend_triple_fault_event(&self, vcpu_id: i32);
    fn unhandled_vm_exit(&self, vcpu_id: i32, exit: VmExitKind);
    fn io_error_event(&self, vcpu_id: i32, error: std::io::Error);
}

#[cfg_attr(test, mockall::automock)]
pub(crate) trait VcpuTaskController {
    fn new_generation(&self);
    fn pause_all(&mut self);
    fn resume_all(&mut self);
    fn exit_all(&mut self);
}

impl VcpuTasks {
    pub(crate) fn new(
        machine: &propolis::Machine,
        event_handler: Arc<super::vm::SharedVmState>,
        log: slog::Logger,
    ) -> Result<Self, VcpuTaskError> {
        let generation = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for vcpu in machine.vcpus.iter().map(Arc::clone) {
            let (task, ctrl) =
                propolis::tasks::TaskHdl::new_held(Some(vcpu.barrier_fn()));
            let task_log = log.new(slog::o!("vcpu" => vcpu.id));
            let task_event_handler = event_handler.clone();
            let task_gen = generation.clone();
            let thread = std::thread::Builder::new()
                .name(format!("vcpu-{}", vcpu.id))
                .spawn(move || {
                    Self::vcpu_loop(
                        vcpu.as_ref(),
                        task,
                        task_event_handler,
                        task_gen,
                        task_log,
                    )
                })
                .map_err(VcpuTaskError::BackingThreadSpawnFailed)?;
            tasks.push((ctrl, thread));
        }

        Ok(Self { tasks, generation })
    }

    fn vcpu_loop(
        vcpu: &Vcpu,
        task: propolis::tasks::TaskHdl,
        event_handler: Arc<super::vm::SharedVmState>,
        generation: Arc<AtomicUsize>,
        log: slog::Logger,
    ) {
        info!(log, "Starting vCPU thread");
        let mut entry = VmEntry::Run;
        let mut exit = propolis::exits::VmExit::default();
        let mut local_gen = 0;
        loop {
            use propolis::tasks::Event;

            let mut force_exit_when_consistent = false;
            match task.pending_event() {
                Some(Event::Hold) => {
                    if !exit.kind.is_consistent() {
                        // Before the vCPU task can enter the held state, its
                        // associated in-kernel state must be driven to a point
                        // where it is consistent.
                        force_exit_when_consistent = true;
                    } else {
                        info!(log, "vCPU paused");
                        task.hold();
                        info!(log, "vCPU released from hold");

                        // If the VM was reset while the CPU was paused, clear out
                        // any re-entry reasons from the exit that occurred prior to
                        // the pause.
                        let current_gen = generation.load(Ordering::Acquire);
                        if local_gen != current_gen {
                            entry = VmEntry::Run;
                            local_gen = current_gen;
                        }

                        // This hold might have been satisfied by a request for the
                        // CPU to exit. Check for other pending events before
                        // re-entering the guest.
                        continue;
                    }
                }
                Some(Event::Exit) => break,
                None => {}
            }

            exit = match vcpu.run(&entry, force_exit_when_consistent) {
                Err(e) => {
                    event_handler.io_error_event(vcpu.id, e);
                    entry = VmEntry::Run;
                    continue;
                }
                Ok(exit) => exit,
            };

            entry = vcpu.process_vmexit(&exit).unwrap_or_else(|| {
                match exit.kind {
                    VmExitKind::Inout(pio) => {
                        debug!(&log, "Unhandled pio {:x?}", pio;
                                       "rip" => exit.rip);
                        VmEntry::InoutFulfill(exits::InoutRes::emulate_failed(
                            &pio,
                        ))
                    }
                    VmExitKind::Mmio(mmio) => {
                        debug!(&log, "Unhandled mmio {:x?}", mmio;
                                       "rip" => exit.rip);
                        VmEntry::MmioFulfill(exits::MmioRes::emulate_failed(
                            &mmio,
                        ))
                    }
                    VmExitKind::Rdmsr(msr) => {
                        debug!(&log, "Unhandled rdmsr {:08x}", msr;
                                       "rip" => exit.rip);
                        let _ = vcpu.set_reg(
                            bhyve_api::vm_reg_name::VM_REG_GUEST_RAX,
                            0,
                        );
                        let _ = vcpu.set_reg(
                            bhyve_api::vm_reg_name::VM_REG_GUEST_RDX,
                            0,
                        );
                        VmEntry::Run
                    }
                    VmExitKind::Wrmsr(msr, val) => {
                        debug!(&log, "Unhandled wrmsr {:08x} <- {:08x}", msr, val;
                                       "rip" => exit.rip);
                        VmEntry::Run
                    }
                    VmExitKind::Suspended(SuspendDetail { kind, when }) => {
                        match kind {
                            exits::Suspend::Halt => {
                                event_handler.suspend_halt_event(when);
                            }
                            exits::Suspend::Reset => {
                                event_handler.suspend_reset_event(when);
                            }
                            exits::Suspend::TripleFault(vcpuid) => {
                                if vcpuid == -1 || vcpuid == vcpu.id {
                                    event_handler
                                        .suspend_triple_fault_event(vcpu.id, when);
                                }
                            }
                        }

                        // This vCPU will not successfully re-enter the guest
                        // until the state worker does something about the
                        // suspend condition, so hold the task until it does so.
                        // Note that this blocks the task immediately.
                        //
                        // N.B.
                        // This usage assumes that it is safe for the VM
                        // controller to ask the task to hold again (which may
                        // occur if a separate pausing event is serviced in
                        // parallel on the state worker).
                        task.force_hold();
                        VmEntry::Run
                    }
                    _ => {
                        event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                        VmEntry::Run
                    }
                }
            });
        }
        info!(log, "Exiting vCPU thread for CPU {}", vcpu.id);
    }
}

impl VcpuTaskController for VcpuTasks {
    fn pause_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.hold().unwrap();
        }
    }

    fn new_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn resume_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.run().unwrap();
        }
    }

    fn exit_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.exit();
        }

        for thread in self.tasks.drain(..) {
            thread.1.join().unwrap();
        }
    }
}
