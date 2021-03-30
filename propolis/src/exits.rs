//! Describes transitions from VMs to the VMM.

use std::convert::TryFrom;
use std::os::raw::c_void;

use bhyve_api::{
    vm_entry, vm_entry_cmds, vm_entry_payload, vm_exit, vm_exitcode,
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
impl From<&vm_exit> for VmExit {
    fn from(exit: &vm_exit) -> Self {
        VmExit {
            rip: exit.rip,
            inst_len: exit.inst_length as u8,
            kind: VmExitKind::from(exit),
        }
    }
}

#[derive(Debug)]
pub struct IoPort {
    pub port: u16,
    pub bytes: u8,
}

#[derive(Debug)]
pub enum InoutReq {
    In(IoPort),
    Out(IoPort, u32),
}

#[derive(Debug)]
pub struct MmioReadReq {
    pub addr: u64,
    pub bytes: u8,
}
#[derive(Debug)]
pub struct MmioWriteReq {
    pub addr: u64,
    pub data: u64,
    pub bytes: u8,
}

#[derive(Debug)]
pub enum MmioReq {
    Read(MmioReadReq),
    Write(MmioWriteReq),
}

#[derive(Debug)]
pub enum VmExitKind {
    Vmx,
    Bogus,
    ReqIdle,
    Inout(InoutReq),
    Mmio(MmioReq),
    Rdmsr(u32),
    Wrmsr(u32, u64),
    Unknown(i32),
}
impl VmExitKind {
    pub fn code(&self) -> i32 {
        match self {
            VmExitKind::Vmx => vm_exitcode::VM_EXITCODE_VMX as i32,
            VmExitKind::Bogus => vm_exitcode::VM_EXITCODE_BOGUS as i32,
            VmExitKind::ReqIdle => vm_exitcode::VM_EXITCODE_REQIDLE as i32,
            VmExitKind::Inout(_) => vm_exitcode::VM_EXITCODE_INOUT as i32,
            VmExitKind::Mmio(_) => vm_exitcode::VM_EXITCODE_MMIO as i32,
            VmExitKind::Rdmsr(_) => vm_exitcode::VM_EXITCODE_RDMSR as i32,
            VmExitKind::Wrmsr(_, _) => vm_exitcode::VM_EXITCODE_WRMSR as i32,
            VmExitKind::Unknown(code) => *code,
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
            vm_exitcode::VM_EXITCODE_VMX => {
                // TODO: Populate and use this exit type.
                let _vmx = unsafe { &exit.u.vmx };
                VmExitKind::Vmx
            }
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
            c => VmExitKind::Unknown(c as i32),
        }
    }
}

pub enum InoutRes {
    In(IoPort, u32),
    Out(IoPort),
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

pub enum VmEntry {
    Run,
    InoutFulfill(InoutRes),
    MmioFulFill(MmioRes),
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
            VmEntry::MmioFulFill(res) => {
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
