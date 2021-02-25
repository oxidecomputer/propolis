pub extern crate bhyve_api;
extern crate dladm;
extern crate viona_api;

#[macro_use]
extern crate bitflags;
extern crate byteorder;

pub mod block;
pub mod chardev;
pub mod common;
pub mod dispatch;
pub mod exits;
pub mod hw;
pub mod instance;
pub mod intr_pins;
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

        let exit = vcpu.run(&next_entry).unwrap();
        //println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
        match exit.kind {
            VmExitKind::Bogus => {
                //println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
                next_entry = VmEntry::Run
            }
            VmExitKind::ReqIdle => {
                // another thread came in to use this vCPU
                // it is likely to push us out for a barrier
                next_entry = VmEntry::Run
            }
            VmExitKind::Inout(io) => match io {
                InoutReq::Out(io, val) => {
                    mctx.with_pio(|b| {
                        b.handle_out(io.port, io.bytes, val, ctx)
                    });
                    next_entry = VmEntry::InoutFulfill(InoutRes::Out(io));
                }
                InoutReq::In(io) => {
                    let val =
                        mctx.with_pio(|b| b.handle_in(io.port, io.bytes, ctx));
                    next_entry = VmEntry::InoutFulfill(InoutRes::In(io, val));
                }
            },
            VmExitKind::Mmio(mmio) => match mmio {
                MmioReq::Read(read) => {
                    let val = mctx.with_mmio(|b| {
                        b.handle_read(read.addr as usize, read.bytes, ctx)
                    });
                    next_entry =
                        VmEntry::MmioFulFill(MmioRes::Read(MmioReadRes {
                            addr: read.addr,
                            bytes: read.bytes,
                            data: val,
                        }));
                }
                MmioReq::Write(write) => {
                    mctx.with_mmio(|b| {
                        b.handle_write(
                            write.addr as usize,
                            write.bytes,
                            write.data,
                            ctx,
                        )
                    });
                    next_entry =
                        VmEntry::MmioFulFill(MmioRes::Write(MmioWriteRes {
                            addr: write.addr,
                            bytes: write.bytes,
                        }));
                }
            },
            VmExitKind::Rdmsr(msr) => {
                println!("rdmsr({:x})", msr);
                // XXX just emulate with 0 for now
                vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RAX, 0).unwrap();
                vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RDX, 0).unwrap();
                next_entry = VmEntry::Run
            }
            VmExitKind::Wrmsr(msr, val) => {
                println!("wrmsr({:x}, {:x})", msr, val);
                next_entry = VmEntry::Run
            }
            _ => panic!("unrecognized exit: {:?}", exit.kind),
        }
    }
}
