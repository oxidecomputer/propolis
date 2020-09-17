use std::convert::TryFrom;

use bhyve_api::{vm_exit, vm_exitcode};

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
pub enum InoutOp {
    In(IoPort),
    Out(IoPort, u32),
}

#[derive(Debug)]
pub enum VmExitKind {
    Bogus,
    Inout(InoutOp),
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
                let inout =  unsafe { &exit.u.inout };
                let port = IoPort {
                    port: inout.port,
                    bytes: inout.bytes,
                };
                if inout.flags & bhyve_api::INOUT_IN != 0 {
                    VmExitKind::Inout(InoutOp::In(port))
                } else {
                    VmExitKind::Inout(InoutOp::Out(port, inout.eax))
                }
            },
            c => VmExitKind::Unknown(c as i32),
        }
    }
}
