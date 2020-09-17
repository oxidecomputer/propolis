extern crate bhyve_api;
extern crate pico_args;
mod vm;

use std::fs::File;
use vm::VmHdl;

const PAGE_OFFSET: u64 = 0xfff;

struct Opts {
    rom: String,
    vmname: String,
}

fn parse_args() -> Opts {
    let mut args = pico_args::Arguments::from_env();
    let rom: String = args.value_from_str("-r").unwrap();
    let vmname: String = args.free().unwrap().pop().unwrap();
    Opts {
        rom,
        vmname,
    }
}

fn init_bootrom(vm: &mut VmHdl, rom: &str) {
    let mut fp = File::open(rom).unwrap();
    let len = fp.metadata().unwrap().len();
    if len & PAGE_OFFSET != 0 {
        panic!("bad rom length {}", len);
    }
    vm.setup_bootrom(len as usize).unwrap();
    vm.populate_bootrom(&mut fp, len as usize).unwrap();
}

fn main() {
    let opts = parse_args();

    let mut vm = vm::create_vm(&opts.vmname).unwrap();

    println!("vm {} created", &opts.vmname);
    vm.setup_memory(512 * 1024 * 1024).unwrap();

    init_bootrom(&mut vm, &opts.rom);

    drop(vm);
}
