use std::convert::TryFrom;
use std::os::raw::c_void;

use bhyve_api::{vm_entry, vm_entry_cmds, vm_entry_payload, vm_exit, vm_exitcode};

pub struct VmExit {
    pub rip: u64,
    pub inst_len: u8,
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
pub enum VmExitKind {
    Bogus,
    Inout(InoutReq),
    Unknown(i32),
}
impl From<&vm_exit> for VmExitKind {
    fn from(exit: &vm_exit) -> Self {
        let code = match vm_exitcode::try_from(exit.exitcode) {
            Err(_) => return VmExitKind::Unknown(exit.exitcode),
            Ok(c) => c,
        };
        match code {
            vm_exitcode::VM_EXITCODE_BOGUS => VmExitKind::Bogus,
            vm_exitcode::VM_EXITCODE_INOUT => {
                let inout = unsafe { &exit.u.inout };
                let port = IoPort {
                    port: inout.port,
                    bytes: inout.bytes,
                };
                if inout.flags & bhyve_api::INOUT_IN != 0 {
                    VmExitKind::Inout(InoutReq::In(port))
                } else {
                    VmExitKind::Inout(InoutReq::Out(port, inout.eax))
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

pub enum VmEntry {
    Run,
    InoutComplete(InoutRes),
}
impl VmEntry {
    pub fn to_raw(&self, cpuid: i32, exit_ptr: *mut vm_exit) -> vm_entry {
        let mut payload = vm_entry_payload::default();
        let cmd = match self {
            VmEntry::Run => vm_entry_cmds::VEC_DEFAULT,
            VmEntry::InoutComplete(res) => {
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
                vm_entry_cmds::VEC_COMPLETE_INOUT
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
