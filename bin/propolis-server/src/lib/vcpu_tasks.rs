//! Tasks for vCPU backing threads and controls for them.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use propolis::{
    bhyve_api,
    exits::{self, VmExitKind},
    vcpu::Vcpu,
    VmEntry, VmError,
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

impl VcpuTasks {
    pub(crate) fn new(
        instance: propolis::instance::InstanceGuard,
        event_handler: Arc<super::vm::WorkerState>,
        log: slog::Logger,
    ) -> Result<Self, VcpuTaskError> {
        let generation = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for vcpu in instance.machine().vcpus.iter().map(Arc::clone) {
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

    pub fn pause_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.hold().unwrap();
        }
    }

    pub fn new_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn resume_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.run().unwrap();
        }
    }

    pub fn exit_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.exit();
        }

        for thread in self.tasks.drain(..) {
            thread.1.join().unwrap();
        }
    }

    fn vcpu_loop(
        vcpu: &Vcpu,
        task: propolis::tasks::TaskHdl,
        event_handler: Arc<super::vm::WorkerState>,
        generation: Arc<AtomicUsize>,
        log: slog::Logger,
    ) {
        info!(log, "Starting vCPU thread");
        let mut entry = VmEntry::Run;
        let mut local_gen = 0;
        loop {
            use propolis::tasks::Event;
            match task.pending_event() {
                Some(Event::Hold) => {
                    info!(log, "vCPU paused");
                    task.hold();

                    // If the VM was reset while the CPU was paused, clear out
                    // any re-entry reasons from the exit that occurred prior to
                    // the pause.
                    let current_gen = generation.load(Ordering::Acquire);
                    if local_gen != current_gen {
                        entry = VmEntry::Run;
                        local_gen = current_gen;
                    }
                }
                Some(Event::Exit) => break,
                None => {}
            }

            entry = match propolis::vcpu_process(vcpu, &entry, &log) {
                Ok(next_entry) => next_entry,
                Err(e) => match e {
                    VmError::Unhandled(exit) => match exit.kind {
                        VmExitKind::Inout(pio) => {
                            debug!(&log, "Unhandled pio {:?}", pio;
                                   "rip" => exit.rip);
                            VmEntry::InoutFulfill(
                                exits::InoutRes::emulate_failed(&pio),
                            )
                        }
                        VmExitKind::Mmio(mmio) => {
                            debug!(&log, "Unhandled mmio {:?}", mmio;
                                   "rip" => exit.rip);
                            VmEntry::MmioFulfill(
                                exits::MmioRes::emulate_failed(&mmio),
                            )
                        }
                        VmExitKind::Rdmsr(msr) => {
                            debug!(&log, "Unhandled rdmsr {:x}", msr;
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
                            debug!(&log, "Unhandled wrmsr {:x}", msr;
                                   "val" => val,
                                   "rip" => exit.rip);
                            VmEntry::Run
                        }
                        _ => {
                            event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                            VmEntry::Run
                        }
                    },
                    VmError::Suspended(suspend) => {
                        match suspend {
                            exits::Suspend::Halt => {
                                event_handler.suspend_halt_event(vcpu.id);
                            }
                            exits::Suspend::Reset => {
                                event_handler.suspend_reset_event(vcpu.id);
                            }
                            exits::Suspend::TripleFault => {
                                event_handler
                                    .suspend_triple_fault_event(vcpu.id);
                            }
                        }

                        // This vCPU will not successfully re-enter the guest
                        // until the state worker does something about the
                        // suspend condition, so hold the task until it does so.
                        // Note that this blocks the task immediately.
                        //
                        // N.B. This usage assumes that it is safe for the VM
                        //      controller to ask the task to hold again (which
                        //      may occur if a separate pausing event is
                        //      serviced in parallel on the state worker).
                        task.force_hold();
                        VmEntry::Run
                    }
                    VmError::Io(e) => {
                        event_handler.io_error_event(vcpu.id, e);
                        VmEntry::Run
                    }
                },
            }
        }
        info!(log, "Exiting vCPU thread for CPU {}", vcpu.id);
    }
}
