extern crate bhyve_api;
extern crate pico_args;
#[macro_use]
extern crate bitflags;

mod devices;
mod exits;
mod inout;
mod pci;
mod util;
mod vm;

use std::fs::File;
use std::sync::Arc;

use bhyve_api::vm_reg_name;
use exits::*;
use vm::{VcpuCtx, VmCtx};

use devices::rtc::Rtc;
use devices::uart::{LpcUart, COM1_IRQ, COM1_PORT};
use inout::InoutBus;
use pci::{PciBDF, PciBus, PciDev, PORT_PCI_CONFIG_ADDR, PORT_PCI_CONFIG_DATA};

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
    let bus_pci = Arc::new(PciBus::new());
    let lpc_pcidev = PciDev::new(0x8086, 0x7000, 0x06, 0x01);

    bus_pio.register(COM1_PORT, 8, com1.clone());
    bus_pio.register(PORT_PCI_CONFIG_ADDR, 4, bus_pci.clone());
    bus_pio.register(PORT_PCI_CONFIG_DATA, 4, bus_pci.clone());
    bus_pci.register(PciBDF::new(0, 31, 0), lpc_pcidev);

    loop {
        let exit = cpu.run(&next_entry).unwrap();
        //println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
        match exit.kind {
            VmExitKind::Bogus => {
                //println!("rip:{:x} exit: {:?}", exit.rip, exit.kind);
                next_entry = VmEntry::Run
            }
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

    let lowmem: usize = 512 * 1024 * 1024;
    println!("vm {} created", &opts.vmname);
    vm.setup_memory(lowmem as u64).unwrap();

    init_bootrom(&mut vm, &opts.rom);

    Rtc::set_time(&vm).unwrap();
    Rtc::store_memory_sizing(&vm, lowmem, None).unwrap();

    let mut vcpu0 = vm.vcpu(0);

    vcpu0.reboot_state().unwrap();
    vcpu0.activate().unwrap();

    run_loop(&mut vcpu0, 0xfff0);

    drop(vm);
}
