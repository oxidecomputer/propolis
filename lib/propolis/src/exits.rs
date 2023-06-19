//! Describes transitions from VMs to the VMM.

use std::convert::TryFrom;
use std::os::raw::c_void;

use bhyve_api::{
    vm_entry, vm_entry_cmds, vm_entry_payload, vm_exit, vm_exitcode,
    vm_suspend_how,
};

/// Describes the reason for exiting execution of a vCPU.
pub struct VmExit {
    /// The instruction pointer of the guest at the time of exit.
    pub rip: u64,
    /// The length of the instruction which triggered the exit.
    /// Zero if inapplicable to the exit or unknown.
    pub inst_len: u8,
    /// Describes the reason for triggering an exit.
    pub kind: VmExitKind,
}
impl Default for VmExit {
    fn default() -> Self {
        Self { rip: 0, inst_len: 0, kind: VmExitKind::Bogus }
    }
}
impl From<&vm_exit> for VmExit {
    fn from(exit: &vm_exit) -> Self {
        VmExit {
            rip: exit.rip,
            inst_len: exit.inst_length as u8,
            kind: VmExitKind::from(exit),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct IoPort {
    pub port: u16,
    pub bytes: u8,
}

#[derive(Copy, Clone, Debug)]
pub enum InoutReq {
    In(IoPort),
    Out(IoPort, u32),
}

#[derive(Copy, Clone, Debug)]
pub struct MmioReadReq {
    pub addr: u64,
    pub bytes: u8,
}
#[derive(Copy, Clone, Debug)]
pub struct MmioWriteReq {
    pub addr: u64,
    pub data: u64,
    pub bytes: u8,
}

#[derive(Copy, Clone, Debug)]
pub enum MmioReq {
    Read(MmioReadReq),
    Write(MmioWriteReq),
}

#[derive(Copy, Clone, Debug)]
pub struct SvmDetail {
    pub exit_code: u64,
    pub info1: u64,
    pub info2: u64,
}
#[derive(Copy, Clone, Debug)]
pub struct VmxDetail {
    pub status: i32,
    pub exit_reason: u32,
    pub exit_qualification: u64,
    pub inst_type: i32,
    pub inst_error: i32,
}
impl From<&bhyve_api::vm_exit_vmx> for VmxDetail {
    fn from(raw: &bhyve_api::vm_exit_vmx) -> Self {
        Self {
            status: raw.status,
            exit_reason: raw.exit_reason,
            exit_qualification: raw.exit_qualification,
            inst_type: raw.inst_type,
            inst_error: raw.inst_error,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct InstEmul {
    pub inst_data: [u8; 15],
    pub len: u8,
}
impl InstEmul {
    pub fn bytes(&self) -> &[u8] {
        &self.inst_data[..usize::min(self.inst_data.len(), self.len as usize)]
    }
}
impl From<&bhyve_api::vm_inst_emul> for InstEmul {
    fn from(raw: &bhyve_api::vm_inst_emul) -> Self {
        let mut res = Self { inst_data: [0u8; 15], len: raw.num_valid };
        assert!(res.len as usize <= res.inst_data.len());
        res.inst_data.copy_from_slice(&raw.inst[..]);

        res
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Suspend {
    Halt,
    Reset,
    TripleFault,
}

#[derive(Copy, Clone, Debug)]
pub enum VmExitKind {
    Bogus,
    ReqIdle,
    Inout(InoutReq),
    Mmio(MmioReq),
    Rdmsr(u32),
    Wrmsr(u32, u64),
    VmxError(VmxDetail),
    SvmError(SvmDetail),
    Suspended(Suspend),
    InstEmul(InstEmul),
    Debug,
    Paging(u64, i32),
    Unknown(i32),
}
impl VmExitKind {
    /// Get the raw `VM_EXITCODE` corresponding to this exit kind
    pub const fn code(&self) -> i32 {
        match self {
            VmExitKind::Bogus => vm_exitcode::VM_EXITCODE_BOGUS as i32,
            VmExitKind::ReqIdle => vm_exitcode::VM_EXITCODE_REQIDLE as i32,
            VmExitKind::Inout(_) => vm_exitcode::VM_EXITCODE_INOUT as i32,
            VmExitKind::Mmio(_) => vm_exitcode::VM_EXITCODE_MMIO as i32,
            VmExitKind::Rdmsr(_) => vm_exitcode::VM_EXITCODE_RDMSR as i32,
            VmExitKind::Wrmsr(_, _) => vm_exitcode::VM_EXITCODE_WRMSR as i32,
            VmExitKind::VmxError(_) => vm_exitcode::VM_EXITCODE_VMX as i32,
            VmExitKind::SvmError(_) => vm_exitcode::VM_EXITCODE_SVM as i32,
            VmExitKind::InstEmul(_) => {
                vm_exitcode::VM_EXITCODE_INST_EMUL as i32
            }
            VmExitKind::Suspended(_) => {
                vm_exitcode::VM_EXITCODE_SUSPENDED as i32
            }
            VmExitKind::Debug => vm_exitcode::VM_EXITCODE_DEBUG as i32,
            VmExitKind::Paging(_, _) => vm_exitcode::VM_EXITCODE_PAGING as i32,
            VmExitKind::Unknown(code) => *code,
        }
    }

    /// Is the vCPU in a consistent (for save/restore or migration) state at the
    /// point when this VM-exit was taken?
    pub const fn is_consistent(&self) -> bool {
        match self {
            // These exitcodes represent conditions unrelated to behavior of the
            // guest vCPU itself, but rather conditions of the host VMM, such as
            // scheduler contention for the CPU, or other software requests that
            // the thread exit to userspace.
            //
            // The checks which would emit such codes are performed only after
            // the rest of the vCPU state is made consistent prior to entry into
            // VM context.
            VmExitKind::Bogus | VmExitKind::ReqIdle | VmExitKind::Debug => true,

            // When the vCPU(s) enter the suspended state, no further forward
            // progress can be made until the instance is reset.
            VmExitKind::Suspended(_) => true,

            // The instruction emulation exits, by their nature, leave the vCPU
            // in an inconsistent state until they can be completed
            VmExitKind::Inout(_)
            | VmExitKind::Mmio(_)
            | VmExitKind::Rdmsr(_)
            | VmExitKind::Wrmsr(_, _)
            | VmExitKind::InstEmul(_) => false,

            // Unhandled paging exits are likely terminal for the instance.
            VmExitKind::Paging(_, _) => true,

            // Unhandled errors or exit codes indicate a terminal state for the
            // entire instance.
            VmExitKind::VmxError(_)
            | VmExitKind::SvmError(_)
            | VmExitKind::Unknown(_) => true,
        }
    }
}
impl From<&vm_exit> for VmExitKind {
    fn from(exit: &vm_exit) -> Self {
        let code = match vm_exitcode::try_from(exit.exitcode) {
            Err(_) => return VmExitKind::Unknown(exit.exitcode),
            Ok(c) => c,
        };
        match code {
            vm_exitcode::VM_EXITCODE_BOGUS => VmExitKind::Bogus,
            vm_exitcode::VM_EXITCODE_REQIDLE => VmExitKind::ReqIdle,
            vm_exitcode::VM_EXITCODE_INOUT => {
                let inout = unsafe { &exit.u.inout };
                let port = IoPort { port: inout.port, bytes: inout.bytes };
                if inout.flags & bhyve_api::INOUT_IN != 0 {
                    VmExitKind::Inout(InoutReq::In(port))
                } else {
                    VmExitKind::Inout(InoutReq::Out(port, inout.eax))
                }
            }
            vm_exitcode::VM_EXITCODE_RDMSR => {
                let msr = unsafe { &exit.u.msr };
                VmExitKind::Rdmsr(msr.code)
            }
            vm_exitcode::VM_EXITCODE_WRMSR => {
                let msr = unsafe { &exit.u.msr };
                VmExitKind::Wrmsr(msr.code, msr.wval)
            }
            vm_exitcode::VM_EXITCODE_MMIO => {
                let mmio = unsafe { &exit.u.mmio };
                if mmio.read != 0 {
                    VmExitKind::Mmio(MmioReq::Read(MmioReadReq {
                        addr: mmio.gpa,
                        bytes: mmio.bytes,
                    }))
                } else {
                    VmExitKind::Mmio(MmioReq::Write(MmioWriteReq {
                        addr: mmio.gpa,
                        data: mmio.data,
                        bytes: mmio.bytes,
                    }))
                }
            }
            vm_exitcode::VM_EXITCODE_VMX => {
                let vmx = unsafe { &exit.u.vmx };
                VmExitKind::VmxError(VmxDetail::from(vmx))
            }
            vm_exitcode::VM_EXITCODE_SVM => {
                let svm = unsafe { &exit.u.svm };
                VmExitKind::SvmError(SvmDetail {
                    exit_code: svm.exitcode,
                    info1: svm.exitinfo1,
                    info2: svm.exitinfo2,
                })
            }
            vm_exitcode::VM_EXITCODE_SUSPENDED => {
                let detail = unsafe { exit.u.suspend };
                match vm_suspend_how::try_from(detail as u32) {
                    Ok(vm_suspend_how::VM_SUSPEND_RESET) => {
                        VmExitKind::Suspended(Suspend::Reset)
                    }
                    Ok(vm_suspend_how::VM_SUSPEND_POWEROFF)
                    | Ok(vm_suspend_how::VM_SUSPEND_HALT) => {
                        VmExitKind::Suspended(Suspend::Halt)
                    }
                    Ok(vm_suspend_how::VM_SUSPEND_TRIPLEFAULT) => {
                        VmExitKind::Suspended(Suspend::TripleFault)
                    }
                    Ok(vm_suspend_how::VM_SUSPEND_NONE) | Err(_) => {
                        panic!("invalid vm_suspend_how: {}", detail);
                    }
                }
            }
            vm_exitcode::VM_EXITCODE_INST_EMUL => {
                let inst = unsafe { &exit.u.inst_emul };
                VmExitKind::InstEmul(InstEmul::from(inst))
            }
            vm_exitcode::VM_EXITCODE_PAGING => {
                let paging = unsafe { &exit.u.paging };
                // The Paging exit should probably be transformed into an
                // attempted-MMIO exit to make handling easier, but until then
                // we just pass the buck.
                VmExitKind::Paging(paging.gpa, paging.fault_type)
            }
            vm_exitcode::VM_EXITCODE_DEBUG => VmExitKind::Debug,

            vm_exitcode::VM_EXITCODE_TASK_SWITCH => {
                // Intel CPUs do not emulate x86 hardware task switching, so it
                // is left to userspace.
                todo!("Implement task-switching emulation on Intel")
            }
            vm_exitcode::VM_EXITCODE_HLT | vm_exitcode::VM_EXITCODE_PAUSE => {
                // Until propolis is changed to request userspace exits for HLT
                // or PAUSE, we do not ever expect to see them.
                panic!("Unexpected {:?}", code);
            }
            vm_exitcode::VM_EXITCODE_BPT | vm_exitcode::VM_EXITCODE_MTRAP => {
                // Propolis is not using VMX breakpoints or mtraps (yet)
                panic!("Unexpected {:?}", code);
            }
            vm_exitcode::VM_EXITCODE_MWAIT
            | vm_exitcode::VM_EXITCODE_MONITOR
            | vm_exitcode::VM_EXITCODE_VMINSN
            | vm_exitcode::VM_EXITCODE_IOAPIC_EOI
            | vm_exitcode::VM_EXITCODE_MMIO_EMUL
            | vm_exitcode::VM_EXITCODE_HT
            | vm_exitcode::VM_EXITCODE_RUN_STATE => {
                // These exitcodes are used (and handled) internally by bhyve
                // and should never be emitted to userspace.
                panic!("Unexpected internal exit: {:?}", code);
            }
            c => VmExitKind::Unknown(c as i32),
        }
    }
}

pub enum InoutRes {
    In(IoPort, u32),
    Out(IoPort),
}
impl InoutRes {
    /// Emit result equivalent to failed IO port operation.  Reads (INx) yield
    /// all 1s, while writes are ignored.
    pub fn emulate_failed(req: &InoutReq) -> Self {
        match req {
            InoutReq::In(port) => InoutRes::In(*port, !0u32),
            InoutReq::Out(port, _data) => InoutRes::Out(*port),
        }
    }
}

pub struct MmioReadRes {
    pub addr: u64,
    pub data: u64,
    pub bytes: u8,
}
pub struct MmioWriteRes {
    pub addr: u64,
    pub bytes: u8,
}

pub enum MmioRes {
    Read(MmioReadRes),
    Write(MmioWriteRes),
}
impl MmioRes {
    /// Emit result equivalent to failed MMIO operation.  Reads yield all 1s,
    /// while writes are ignored.
    pub fn emulate_failed(req: &MmioReq) -> Self {
        match req {
            MmioReq::Read(read) => MmioRes::Read(MmioReadRes {
                addr: read.addr,
                data: !0u64,
                bytes: read.bytes,
            }),
            MmioReq::Write(write) => MmioRes::Write(MmioWriteRes {
                addr: write.addr,
                bytes: write.bytes,
            }),
        }
    }
}

pub enum VmEntry {
    Run,
    InoutFulfill(InoutRes),
    MmioFulfill(MmioRes),
}
impl VmEntry {
    pub fn to_raw(&self, cpuid: i32, exit_ptr: *mut vm_exit) -> vm_entry {
        let mut payload = vm_entry_payload::default();
        let cmd = match self {
            VmEntry::Run => vm_entry_cmds::VEC_DEFAULT,
            VmEntry::InoutFulfill(res) => {
                let io = match res {
                    InoutRes::In(io, val) => {
                        payload.inout.flags = bhyve_api::INOUT_IN;
                        payload.inout.eax = *val;
                        io
                    }
                    InoutRes::Out(io) => {
                        payload.inout.flags = 0;
                        payload.inout.eax = 0;
                        io
                    }
                };
                payload.inout.port = io.port;
                payload.inout.bytes = io.bytes;
                vm_entry_cmds::VEC_FULFILL_INOUT
            }
            VmEntry::MmioFulfill(res) => {
                let (addr, bytes) = match res {
                    MmioRes::Read(read) => {
                        payload.mmio.read = 1;
                        payload.mmio.data = read.data;
                        (read.addr, read.bytes)
                    }
                    MmioRes::Write(write) => (write.addr, write.bytes),
                };
                payload.mmio.gpa = addr;
                payload.mmio.bytes = bytes;
                vm_entry_cmds::VEC_FULFILL_MMIO
            }
        };
        vm_entry {
            cpuid,
            cmd: cmd as u32,
            u: payload,
            exit_data: exit_ptr as *mut c_void,
        }
    }
}
