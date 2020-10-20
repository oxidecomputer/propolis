extern crate bhyve_api;
extern crate pico_args;
#[macro_use]
extern crate bitflags;
extern crate byteorder;

mod block;
mod common;
mod devices;
mod dispatch;
mod exits;
mod intr_pins;
mod pci;
mod pio;
mod util;
mod vcpu;
mod vmm;

use std::fs::File;
use std::io::{Error, ErrorKind, Read, Result};
use std::path::Path;
use std::sync::Arc;

use bhyve_api::vm_reg_name;
use dispatch::*;
use exits::*;
use vcpu::VcpuHdl;
use vmm::{Machine, MachineCtx};

use pci::PciBDF;

const PAGE_OFFSET: u64 = 0xfff;
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

struct Opts {
    rom: String,
    vmname: String,
    blockdev: Option<String>,
}

fn parse_args() -> Opts {
    let mut args = pico_args::Arguments::from_env();
    let rom: String = args.value_from_str("-r").unwrap();
    let blockdev: Option<String> = args.opt_value_from_str("-b").unwrap();
    let vmname: String = args.free().unwrap().pop().unwrap();
    Opts { rom, vmname, blockdev }
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
                    mctx.with_pio(|b| {
                        b.handle_out(io.port, io.bytes, val, &dctx)
                    });
                    next_entry = VmEntry::InoutComplete(InoutRes::Out(io));
                }
                InoutReq::In(io) => {
                    let val = mctx
                        .with_pio(|b| b.handle_in(io.port, io.bytes, &dctx));
                    next_entry = VmEntry::InoutComplete(InoutRes::In(io, val));
                }
            },
            VmExitKind::Mmio(mmio) => match mmio {
                MmioReq::Read(read) => {
                    println!(
                        "unhandled mmio read {:x} {}",
                        read.addr, read.bytes
                    );
                    next_entry =
                        VmEntry::MmioComplete(MmioRes::Read(MmioReadRes {
                            addr: read.addr,
                            bytes: read.bytes,
                            // XXX fake read for now
                            data: 0,
                        }));
                }
                MmioReq::Write(write) => {
                    println!(
                        "unhandled mmio write {:x} {} {:x}",
                        write.addr, write.bytes, write.data
                    );
                    next_entry =
                        VmEntry::MmioComplete(MmioRes::Write(MmioWriteRes {
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

use vmm::{Builder, Prot};

fn build_vm(name: &str, lowmem: usize) -> Result<Arc<Machine>> {
    let vm = Builder::new(name, true)?
        .max_cpus(1)?
        .add_mem_region(0, lowmem, Prot::ALL, "lowmem")?
        .add_rom_region(
            0x1_0000_0000 - MAX_ROM_SIZE,
            MAX_ROM_SIZE,
            Prot::READ | Prot::EXEC,
            "bootrom",
        )?
        .add_mmio_region(0xc0000000 as usize, 0x20000000 as usize, "dev32")?
        .add_mmio_region(0xe0000000 as usize, 0x10000000 as usize, "pcicfg")?
        .add_mmio_region(
            vmm::MAX_SYSMEM,
            vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
            "dev64",
        )?
        .finalize()?;
    Ok(vm)
}

fn open_bootrom(path: &str) -> Result<(File, usize)> {
    let mut fp = File::open(path)?;
    let len = fp.metadata()?.len();
    if len & PAGE_OFFSET != 0 {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "rom {} length {:x} not aligned to {:x}",
                path,
                len,
                PAGE_OFFSET + 1
            ),
        ))
    } else {
        Ok((fp, len as usize))
    }
}

fn main() {
    let opts = parse_args();

    let lowmem: usize = 512 * 1024 * 1024;

    let vm = build_vm(&opts.vmname, lowmem).unwrap();
    println!("vm {} created", &opts.vmname);

    let (mut romfp, rom_len) = open_bootrom(&opts.rom).unwrap();
    vm.populate_rom("bootrom", |ptr, region_len| {
        if region_len < rom_len {
            return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
        }
        let offset = region_len - rom_len;
        unsafe {
            let write_ptr = ptr.as_ptr().offset(offset as isize);
            let buf = std::slice::from_raw_parts_mut(write_ptr, rom_len);
            match romfp.read(buf) {
                Ok(n) if n == rom_len => Ok(()),
                Ok(_) => {
                    // TODO: handle short read
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    })
    .unwrap();
    drop(romfp);

    vm.initalize_rtc(lowmem).unwrap();
    vm.wire_pci_root();

    let mctx = MachineCtx::new(&vm);
    let mut dispatch = Dispatcher::new(mctx.clone());
    dispatch.spawn_events().unwrap();

    let pci_hostbridge = devices::piix4::Piix4HostBridge::new();

    let com1_sock = devices::uart::UartSock::bind(Path::new("./ttya")).unwrap();
    dispatch.with_ctx(|ctx| {
        com1_sock.accept_ready(ctx);
    });

    let pci_lpc = vm.create_lpc(|pic, pio| {
        devices::lpc::Piix3Bhyve::new(pic, pio, Arc::clone(&com1_sock))
    });
    pci_lpc.with_inner(|lpc| lpc.set_pir_defaults());

    mctx.with_pci(|pci| pci.attach(PciBDF::new(0, 0, 0), pci_hostbridge));
    mctx.with_pci(|pci| pci.attach(PciBDF::new(0, 31, 0), pci_lpc));

    if let Some(bpath) = opts.blockdev.as_ref() {
        let plain: Arc<block::PlainBdev<devices::virtio::block::Request>> =
            block::PlainBdev::new(bpath).unwrap();

        let vioblk = devices::virtio::VirtioBlock::new(
            0x100,
            Arc::clone(&plain)
                as Arc<dyn block::BlockDev<devices::virtio::block::Request>>,
        );
        mctx.with_pci(|pci| pci.attach(PciBDF::new(0, 4, 0), vioblk));

        plain.start_dispatch("bdev thread".to_string(), &dispatch);
    }

    // with all pci devices attached, place their BARs
    dispatch.with_ctx(|ctx| {
        // hacky nesting
        ctx.mctx.with_pci(|pci| pci.place_bars(ctx));
    });

    let mut vcpu0 = vm.vcpu(0);

    vcpu0.set_default_capabs().unwrap();
    vcpu0.reboot_state().unwrap();
    vcpu0.activate().unwrap();
    vcpu0.set_reg(vm_reg_name::VM_REG_GUEST_RIP, 0xfff0).unwrap();

    // Wait until someone connects to ttya
    com1_sock.wait_for_connect();

    dispatch.spawn_vcpu(vcpu0, run_loop).unwrap();

    dispatch.join();
    drop(vm);
}
