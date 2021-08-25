// Pull in asm!() support for USDT
#![feature(asm)]
#![allow(clippy::style)]

use usdt::dtrace_provider;

// Define probe macros prior to module imports below.
dtrace_provider!("usdt.d", format = "probe_{probe}");

pub extern crate bhyve_api;
pub extern crate usdt;
#[macro_use]
extern crate bitflags;

pub mod block;
pub mod chardev;
pub mod common;
pub mod dispatch;
pub mod exits;
pub mod hw;
pub mod instance;
pub mod intr_pins;
pub mod inventory;
pub mod mmio;
pub mod pio;
pub mod util;
pub mod vcpu;
pub mod vmm;

use bhyve_api::vm_reg_name;
use dispatch::*;
use exits::*;
use vcpu::VcpuHdl;

pub fn vcpu_run_loop(mut vcpu: VcpuHdl, ctx: &mut DispCtx) {
    let mut next_entry = VmEntry::Run;
    loop {
        if ctx.check_yield() {
            break;
        }
        let mctx = &ctx.mctx;

        probe_vm_entry!(|| (vcpu.cpuid() as u32));
        let exit = vcpu.run(&next_entry).unwrap();
        probe_vm_exit!(|| (
            vcpu.cpuid() as u32,
            exit.rip,
            exit.kind.code() as u32
        ));

        next_entry = match exit.kind {
            VmExitKind::Bogus => VmEntry::Run,
            VmExitKind::ReqIdle => {
                // another thread came in to use this vCPU
                // it is likely to push us out for a barrier
                VmEntry::Run
            }
            VmExitKind::Inout(io) => match io {
                InoutReq::Out(io, val) => {
                    mctx.pio().handle_out(io.port, io.bytes, val, ctx);
                    VmEntry::InoutFulfill(InoutRes::Out(io))
                }
                InoutReq::In(io) => {
                    let val = mctx.pio().handle_in(io.port, io.bytes, ctx);
                    VmEntry::InoutFulfill(InoutRes::In(io, val))
                }
            },
            VmExitKind::Mmio(mmio) => match mmio {
                MmioReq::Read(read) => {
                    let val = mctx.mmio().handle_read(
                        read.addr as usize,
                        read.bytes,
                        ctx,
                    );
                    VmEntry::MmioFulFill(MmioRes::Read(MmioReadRes {
                        addr: read.addr,
                        bytes: read.bytes,
                        data: val,
                    }))
                }
                MmioReq::Write(write) => {
                    mctx.mmio().handle_write(
                        write.addr as usize,
                        write.bytes,
                        write.data,
                        ctx,
                    );
                    VmEntry::MmioFulFill(MmioRes::Write(MmioWriteRes {
                        addr: write.addr,
                        bytes: write.bytes,
                    }))
                }
            },
            VmExitKind::Rdmsr(msr) => {
                println!("rdmsr({:x})", msr);
                // XXX just emulate with 0 for now
                vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RAX, 0).unwrap();
                vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDX, 0).unwrap();
                VmEntry::Run
            }
            VmExitKind::Wrmsr(msr, val) => {
                println!("wrmsr({:x}, {:x})", msr, val);
                VmEntry::Run
            }
            VmExitKind::Debug => {
                // Until there is an interface to delay until a vCPU is no
                // longer under control of the debugger, we have no choice but
                // attempt reentry (and probably spin until the debugger is
                // detached from this vCPU).
                VmEntry::Run
            }
            VmExitKind::InstEmul(inst) => {
                panic!(
                    "unemulated instruction {:x?} as rip:{:x}",
                    inst.bytes(),
                    exit.rip
                );
            }
            VmExitKind::Paging(gpa, ftype) => {
                panic!("unhandled paging gpa:{:x} type:{:x}", gpa, ftype);
            }
            VmExitKind::VmxError(detail) => {
                panic!("VMX error: {:?}", detail);
            }
            VmExitKind::SvmError(detail) => {
                panic!("SVM error: {:?}", detail);
            }
            VmExitKind::Suspended(kind) => {
                todo!("drive instance toward {:?} suspension", kind);
            }
            _ => panic!("unrecognized exit: {:?}", exit.kind),
        }
    }
}
