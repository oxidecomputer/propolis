#![allow(clippy::style)]
#![allow(clippy::drop_non_drop)]
#![cfg_attr(usdt_need_asm, feature(asm))]
#![cfg_attr(all(target_os = "macos", usdt_need_asm_sym), feature(asm_sym))]

pub extern crate bhyve_api;
pub extern crate usdt;
#[macro_use]
extern crate bitflags;

pub mod accessors;
pub mod api_version;
pub mod block;
pub mod chardev;
pub mod common;
pub mod exits;
pub mod hw;
pub mod instance;
pub mod intr_pins;
pub mod inventory;
pub mod migrate;
pub mod mmio;
pub mod pio;
pub mod tasks;
pub mod util;
pub mod vcpu;
pub mod vmm;

pub use crate::exits::{VmEntry, VmExit};
pub use crate::instance::Instance;

use bhyve_api::vm_reg_name;
use exits::*;
use vcpu::Vcpu;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn vm_entry(vcpuid: u32) {}
    fn vm_exit(vcpuid: u32, rip: u64, code: u32) {}
}

pub enum VmError {
    Unhandled(VmExit),
    Suspended(Suspend),
    Io(std::io::Error),
}
impl From<std::io::Error> for VmError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

pub fn vcpu_process(
    vcpu: &Vcpu,
    entry: &VmEntry,
    log: &slog::Logger,
) -> Result<VmEntry, VmError> {
    probes::vm_entry!(|| (vcpu.cpuid() as u32));
    let exit = vcpu.run(&entry)?;
    probes::vm_exit!(|| (
        vcpu.cpuid() as u32,
        exit.rip,
        exit.kind.code() as u32
    ));

    match exit.kind {
        VmExitKind::Bogus => Ok(VmEntry::Run),
        VmExitKind::ReqIdle => {
            // another thread came in to use this vCPU it is likely to push
            // us out for a barrier
            Ok(VmEntry::Run)
        }
        VmExitKind::Inout(io) => match io {
            InoutReq::Out(io, val) => vcpu
                .bus_pio
                .handle_out(io.port, io.bytes, val)
                .map(|_| VmEntry::InoutFulfill(InoutRes::Out(io)))
                .map_err(|_| VmError::Unhandled(exit)),
            InoutReq::In(io) => vcpu
                .bus_pio
                .handle_in(io.port, io.bytes)
                .map(|val| VmEntry::InoutFulfill(InoutRes::In(io, val)))
                .map_err(|_| VmError::Unhandled(exit)),
        },
        VmExitKind::Mmio(mmio) => match mmio {
            MmioReq::Read(read) => vcpu
                .bus_mmio
                .handle_read(read.addr as usize, read.bytes)
                .map(|val| {
                    VmEntry::MmioFulfill(MmioRes::Read(MmioReadRes {
                        addr: read.addr,
                        bytes: read.bytes,
                        data: val,
                    }))
                })
                .map_err(|_| VmError::Unhandled(exit)),
            MmioReq::Write(write) => vcpu
                .bus_mmio
                .handle_write(write.addr as usize, write.bytes, write.data)
                .map(|_| {
                    VmEntry::MmioFulfill(MmioRes::Write(MmioWriteRes {
                        addr: write.addr,
                        bytes: write.bytes,
                    }))
                })
                .map_err(|_| VmError::Unhandled(exit)),
        },
        VmExitKind::Rdmsr(msr) => {
            slog::info!(log, "rdmsr"; "msr" => msr);
            // XXX just emulate with 0 for now
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RAX, 0).unwrap();
            vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDX, 0).unwrap();
            Ok(VmEntry::Run)
        }
        VmExitKind::Wrmsr(msr, val) => {
            slog::info!(log, "wrmsr"; "msr" => msr, "value" => val);
            Ok(VmEntry::Run)
        }
        VmExitKind::Debug => {
            // Until there is an interface to delay until a vCPU is no
            // longer under control of the debugger, we have no choice but
            // attempt reentry (and probably spin until the debugger is
            // detached from this vCPU).
            Ok(VmEntry::Run)
        }
        VmExitKind::Suspended(detail) => Err(VmError::Suspended(detail)),

        VmExitKind::InstEmul(_)
        | VmExitKind::Paging(_, _)
        | VmExitKind::VmxError(_)
        | VmExitKind::SvmError(_) => Err(VmError::Unhandled(exit)),
        _ => Err(VmError::Unhandled(exit)),
    }
}
