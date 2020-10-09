extern crate bhyve_api;
extern crate pico_args;
#[macro_use]
extern crate bitflags;
extern crate byteorder;

mod devices;
mod dispatch;
mod exits;
mod intr_pins;
mod machine;
mod pci;
mod pio;
mod types;
mod util;
mod vcpu;
mod vm;

use std::fs::File;
use std::sync::Arc;

use bhyve_api::vm_reg_name;
use dispatch::*;
use exits::*;
use machine::{Machine, MachineCtx};
use vcpu::VcpuHdl;

use pci::PciBDF;

const PAGE_OFFSET: u64 = 0xfff;

struct Opts {
    rom: String,
    vmname: String,
}

fn parse_args() -> Opts {
    let mut args = pico_args::Arguments::from_env();
    let rom: String = args.value_from_str("-r").unwrap();
    let vmname: String = args.free().unwrap().pop().unwrap();
    Opts { rom, vmname }
}

fn run_loop(dctx: DispCtx, mut vcpu: VcpuHdl) {
    let mctx = &dctx.mctx;
    let mut next_entry = VmEntry::Run;
    loop {
        let exit = vcpu.run(&next_entry).unwrap();
        //println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
        match exit.kind {
            VmExitKind::Bogus => {
                //println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
                next_entry = VmEntry::Run
            }
            VmExitKind::Inout(io) => match io {
                InoutReq::Out(io, val) => {
                    mctx.with_pio(|b| b.handle_out(io.port, io.bytes, val));
                    next_entry = VmEntry::InoutComplete(InoutRes::Out(io));
                }
                InoutReq::In(io) => {
                    let val = mctx.with_pio(|b| b.handle_in(io.port, io.bytes));
                    next_entry = VmEntry::InoutComplete(InoutRes::In(io, val));
                }
            },
            _ => panic!("unrecognized exit: {:?}", exit.kind),
        }
    }
}

fn main() {
    let opts = parse_args();

    let hdl = vm::create_vm(&opts.vmname).unwrap();
    println!("vm {} created", &opts.vmname);

    let vm = Machine::new(hdl, 1);

    let lowmem: usize = 512 * 1024 * 1024;
    vm.setup_lowmem(lowmem).unwrap();

    // Setup bootrom
    {
        let mut fp = File::open(&opts.rom).unwrap();
        let len = fp.metadata().unwrap().len();
        if len & PAGE_OFFSET != 0 {
            panic!("bad rom length {}", len);
        }
        vm.setup_bootrom(len as usize).unwrap();
        vm.populate_bootrom(&mut fp, len as usize).unwrap();
    }

    vm.initalize_rtc(lowmem).unwrap();

    vm.wire_pci_root();

    let pci_hostbridge = devices::piix4::Piix4HostBridge::new();
    let pci_lpc =
        vm.create_lpc(|pic, pio| devices::lpc::Piix3Bhyve::new(pic, pio));
    pci_lpc.with_inner(|lpc| lpc.set_pir_defaults());

    let mctx = MachineCtx::new(&vm);
    mctx.with_pci(|pci| pci.attach(PciBDF::new(0, 0, 0), pci_hostbridge));
    mctx.with_pci(|pci| pci.attach(PciBDF::new(0, 31, 0), pci_lpc));

    let mut vcpu0 = vm.vcpu(0);

    vcpu0.reboot_state().unwrap();
    vcpu0.activate().unwrap();
    vcpu0.set_reg(vm_reg_name::VM_REG_GUEST_RIP, 0xfff0).unwrap();

    let mut dispatch = Dispatcher::new(mctx);

    dispatch.spawn_events().unwrap();
    dispatch.spawn_vcpu(vcpu0, run_loop).unwrap();

    dispatch.join();

    drop(vm);
}
