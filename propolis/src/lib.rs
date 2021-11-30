#![allow(clippy::style)]
// Pull in asm!() and asm_sym!() support for USDT
#![cfg_attr(feature = "dtrace-probes", feature(asm))]
#![cfg_attr(
    all(feature = "dtrace-probes", target_os = "macos"),
    feature(asm_sym)
)]
// Pull in `assert_matches` for tests
#![cfg_attr(test, feature(assert_matches))]

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
pub mod migrate;
pub mod mmio;
pub mod pio;
pub mod util;
pub mod vcpu;
pub mod vmm;

use bhyve_api::vm_reg_name;
use dispatch::*;
use exits::*;
use vcpu::VcpuHdl;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn vm_entry(vcpuid: u32) {}
    fn vm_exit(vcpuid: u32, rip: u64, code: u32) {}
}

pub fn vcpu_run_loop(mut vcpu: VcpuHdl, sctx: &mut SyncCtx) {
    let mut next_entry = VmEntry::Run;
    loop {
        if sctx.check_yield() {
            break;
        }
        let ctx = sctx.dispctx();
        let mctx = &ctx.mctx;

        probes::vm_entry!(|| (vcpu.cpuid() as u32));
        let exit = vcpu.run(&next_entry).unwrap();
        probes::vm_exit!(|| (
            vcpu.cpuid() as u32,
            exit.rip,
            exit.kind.code() as u32
        ));

        next_entry = match exit.kind {
            VmExitKind::Bogus => VmEntry::Run,
            VmExitKind::ReqIdle => {
                // another thread came in to use this vCPU it is likely to push
                // us out for a barrier
                VmEntry::Run
            }
            VmExitKind::Inout(io) => match io {
                InoutReq::Out(io, val) => {
                    mctx.pio().handle_out(io.port, io.bytes, val, &ctx);
                    VmEntry::InoutFulfill(InoutRes::Out(io))
                }
                InoutReq::In(io) => {
                    let val = mctx.pio().handle_in(io.port, io.bytes, &ctx);
                    VmEntry::InoutFulfill(InoutRes::In(io, val))
                }
            },
            VmExitKind::Mmio(mmio) => match mmio {
                MmioReq::Read(read) => {
                    let val = mctx.mmio().handle_read(
                        read.addr as usize,
                        read.bytes,
                        &ctx,
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
                        &ctx,
                    );
                    VmEntry::MmioFulFill(MmioRes::Write(MmioWriteRes {
                        addr: write.addr,
                        bytes: write.bytes,
                    }))
                }
            },
            VmExitKind::Rdmsr(msr) => {
                slog::info!(ctx.log, "rdmsr"; "msr" => msr);
                // XXX just emulate with 0 for now
                vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RAX, 0).unwrap();
                vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDX, 0).unwrap();
                VmEntry::Run
            }
            VmExitKind::Wrmsr(msr, val) => {
                slog::info!(ctx.log, "wrmsr"; "msr" => msr, "value" => val);
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
            VmExitKind::Suspended(detail) => {
                let (kind, source) = match detail {
                    Suspend::TripleFault => (
                        instance::SuspendKind::Reset,
                        instance::SuspendSource::TripleFault(
                            vcpu.cpuid() as i32
                        ),
                    ),
                    // Attempt to trigger non-triple-fault suspensions, despite
                    // the fact that such exits are likely due in-process
                    // suspend operations triggered operation by a device or
                    // external request.  This is harmless as redundant
                    // suspension requests are ignored.
                    Suspend::Reset => (
                        instance::SuspendKind::Reset,
                        instance::SuspendSource::Device("cpu"),
                    ),
                    Suspend::Halt => (
                        instance::SuspendKind::Halt,
                        instance::SuspendSource::Device("cpu"),
                    ),
                };
                ctx.trigger_suspend(kind, source);

                // We expect the dispatch machinery to at least cause us to
                // yield, but we still want to attempt to run for the normal
                // reset case.
                VmEntry::Run
            }
            _ => panic!("unrecognized exit: {:?}", exit.kind),
        }
    }
}
