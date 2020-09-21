extern crate aspace;
extern crate bhyve_api;
extern crate pico_args;

mod devices;
mod exits;
mod inout;
mod vm;

use bhyve_api::vm_reg_name;
use exits::*;
use std::fs::File;
use vm::{VcpuCtx, VmCtx};

use devices::uart::{LpcUart, COM1_IRQ, COM1_PORT};
use inout::InoutBus;
use std::sync::Arc;

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

fn init_bootrom(vm: &mut VmCtx, rom: &str) {
    let mut fp = File::open(rom).unwrap();
    let len = fp.metadata().unwrap().len();
    if len & PAGE_OFFSET != 0 {
        panic!("bad rom length {}", len);
    }
    vm.setup_bootrom(len as usize).unwrap();
    vm.populate_bootrom(&mut fp, len as usize).unwrap();
}

fn run_loop(cpu: &mut VcpuCtx, start_rip: u64) {
    cpu.set_reg(vm_reg_name::VM_REG_GUEST_RIP, start_rip)
        .unwrap();
    let mut next_entry = VmEntry::Run;

    let mut bus_pio = InoutBus::new();
    let com1 = Arc::new(LpcUart::new(COM1_IRQ));
    bus_pio.register(COM1_PORT, COM1_PORT + 7, com1.clone());

    loop {
        let exit = cpu.run(&next_entry).unwrap();
        println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
        match exit.kind {
            VmExitKind::Bogus => next_entry = VmEntry::Run,
            VmExitKind::Inout(io) => match io {
                InoutReq::Out(io, val) => {
                    bus_pio.handle_out(io.port, io.bytes, val);
                    next_entry = VmEntry::InoutComplete(InoutRes::Out(io));
                }
                InoutReq::In(io) => {
                    let val = bus_pio.handle_in(io.port, io.bytes);
                    next_entry = VmEntry::InoutComplete(InoutRes::In(io, val));
                }
            },
            _ => panic!("unrecognized exit: {:?}", exit.kind),
        }
    }
}

fn main() {
    let opts = parse_args();

    let mut vm = vm::create_vm(&opts.vmname).unwrap();

    println!("vm {} created", &opts.vmname);
    vm.setup_memory(512 * 1024 * 1024).unwrap();

    init_bootrom(&mut vm, &opts.rom);

    let mut vcpu0 = vm.vcpu(0);

    vcpu0.reboot_state().unwrap();
    vcpu0.activate().unwrap();

    run_loop(&mut vcpu0, 0xfff0);

    drop(vm);
}
