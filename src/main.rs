extern crate bhyve_api;
extern crate dladm;
extern crate viona_api;

#[macro_use]
extern crate bitflags;
extern crate byteorder;
extern crate pico_args;
extern crate serde;
extern crate serde_derive;
extern crate toml;

mod block;
mod chardev;
mod common;
mod config;
mod dispatch;
mod exits;
mod hw;
mod intr_pins;
mod mmio;
mod pio;
mod util;
mod vcpu;
mod vmm;

use std::fs::File;
use std::io::{Error, ErrorKind, Read, Result};
use std::path::Path;
use std::sync::Arc;

use bhyve_api::vm_reg_name;
use chardev::{Sink, Source};
use dispatch::*;
use exits::*;
use hw::chipset::Chipset;
use vcpu::VcpuHdl;
use vmm::{Machine, MachineCtx};

const PAGE_OFFSET: u64 = 0xfff;
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

fn parse_args() -> config::Config {
    let args = pico_args::Arguments::from_env();
    if let Some(cpath) = args.free().ok().map(|mut f| f.pop()).flatten() {
        config::parse(&cpath)
    } else {
        eprintln!("usage: propolis <CONFIG.toml>");
        std::process::exit(libc::EXIT_FAILURE);
    }
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
                    next_entry = VmEntry::InoutFulfill(InoutRes::Out(io));
                }
                InoutReq::In(io) => {
                    let val = mctx
                        .with_pio(|b| b.handle_in(io.port, io.bytes, &dctx));
                    next_entry = VmEntry::InoutFulfill(InoutRes::In(io, val));
                }
            },
            VmExitKind::Mmio(mmio) => match mmio {
                MmioReq::Read(read) => {
                    let val = mctx.with_mmio(|b| {
                        b.handle_read(read.addr as usize, read.bytes, &dctx)
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
                            &dctx,
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

use vmm::{Builder, Prot};

fn build_vm(name: &str, max_cpu: u8, lowmem: usize) -> Result<Arc<Machine>> {
    let vm = Builder::new(name, true)?
        .max_cpus(max_cpu)?
        .add_mem_region(0, lowmem, Prot::ALL, "lowmem")?
        .add_rom_region(
            0x1_0000_0000 - MAX_ROM_SIZE,
            MAX_ROM_SIZE,
            Prot::READ | Prot::EXEC,
            "bootrom",
        )?
        .add_mmio_region(0xc0000000_usize, 0x20000000_usize, "dev32")?
        .add_mmio_region(0xe0000000_usize, 0x10000000_usize, "pcicfg")?
        .add_mmio_region(
            vmm::MAX_SYSMEM,
            vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
            "dev64",
        )?
        .finalize()?;
    Ok(vm)
}

fn open_bootrom(path: &str) -> Result<(File, usize)> {
    let fp = File::open(path)?;
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
    let config = parse_args();

    let vm_name = config.get_name();
    let lowmem: usize = config.get_mem() * 1024 * 1024;
    let cpus = config.get_cpus();

    let vm = build_vm(vm_name, cpus, lowmem).unwrap();
    println!("vm {} created", vm_name);

    let (mut romfp, rom_len) = open_bootrom(config.get_bootrom()).unwrap();
    vm.populate_rom("bootrom", |ptr, region_len| {
        if region_len < rom_len {
            return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
        }
        let offset = region_len - rom_len;
        unsafe {
            let write_ptr = ptr.as_ptr().add(offset);
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

    let mctx = MachineCtx::new(&vm);
    let mut dispatch = Dispatcher::new(mctx.clone());
    dispatch.spawn_events().unwrap();

    let com1_sock = chardev::UDSock::bind(Path::new("./ttya")).unwrap();
    dispatch.with_ctx(|ctx| {
        com1_sock.listen(ctx);
    });

    let chipset = mctx.with_pio(|pio| {
        hw::chipset::i440fx::I440Fx::create(vm.get_hdl(), pio, |lpc| {
            lpc.config_uarts(|com1, com2| {
                com1_sock.attach_sink(Arc::clone(com1) as Arc<dyn Sink>);
                com1_sock.attach_source(Arc::clone(com1) as Arc<dyn Source>);
                com1.source_set_autodiscard(false);

                // XXX: plumb up com2, but until then, just auto-discard
                com2.source_set_autodiscard(true);
            })
        })
    });

    let _dbg = mctx.with_pio(|pio| {
        let debug = std::fs::File::create("debug.out").unwrap();
        let buffered = std::io::LineWriter::new(debug);
        hw::qemu::debug::QemuDebugPort::create(
            Some(Box::new(buffered) as Box<dyn std::io::Write + Send>),
            pio,
        )
    });

    for (name, dev) in config.devs() {
        let driver = &dev.driver as &str;
        let bdf = if driver.starts_with("pci-") {
            config::parse_bdf(
                dev.options.get("pci-path").unwrap().as_str().unwrap(),
            )
        } else {
            None
        };
        match driver {
            "pci-virtio-block" => {
                let disk_path =
                    dev.options.get("disk").unwrap().as_str().unwrap();

                let plain: Arc<block::PlainBdev<hw::virtio::block::Request>> =
                    block::PlainBdev::create(disk_path).unwrap();

                let vioblk = hw::virtio::VirtioBlock::create(
                    0x100,
                    Arc::clone(&plain)
                        as Arc<dyn block::BlockDev<hw::virtio::block::Request>>,
                );
                chipset.pci_attach(bdf.unwrap(), vioblk);

                plain
                    .start_dispatch(format!("bdev-{} thread", name), &dispatch);
            }
            "pci-virtio-viona" => {
                let vnic_name =
                    dev.options.get("vnic").unwrap().as_str().unwrap();

                let hdl = vm.get_hdl();
                let viona = hw::virtio::viona::VirtioViona::create(
                    vnic_name, 0x100, &hdl,
                )
                .unwrap();
                chipset.pci_attach(bdf.unwrap(), viona);
            }
            _ => {
                eprintln!("unrecognized driver: {}", name);
                std::process::exit(libc::EXIT_FAILURE);
            }
        }
    }

    // with all pci devices attached, place their BARs and wire up access to PCI
    // configuration space
    dispatch.with_ctx(|ctx| chipset.pci_finalize(ctx));

    let fwcfg = mctx.with_pio(|pio| hw::qemu::fwcfg::FwCfg::create(pio));
    fwcfg.add_legacy(
        hw::qemu::fwcfg::LegacyId::SmpCpuCount,
        hw::qemu::fwcfg::FixedItem::new_u32(cpus as u32),
    );

    // Spin up non-boot CPUs prior to vCPU 0
    // They will simply block until INIT/SIPI is received
    for n in 1..cpus {
        let mut next_vcpu = vm.vcpu(n as i32);
        next_vcpu.set_default_capabs().unwrap();
        next_vcpu.reboot_state().unwrap();
        next_vcpu.activate().unwrap();
        dispatch.spawn_vcpu(next_vcpu, run_loop).unwrap();
    }

    let mut vcpu0 = vm.vcpu(0);

    vcpu0.set_default_capabs().unwrap();
    vcpu0.reboot_state().unwrap();
    vcpu0.activate().unwrap();
    vcpu0.set_run_state(bhyve_api::VRS_RUN).unwrap();
    vcpu0.set_reg(vm_reg_name::VM_REG_GUEST_RIP, 0xfff0).unwrap();

    // Wait until someone connects to ttya
    com1_sock.wait_for_connect();

    dispatch.spawn_vcpu(vcpu0, run_loop).unwrap();

    dispatch.join();
    drop(vm);
}
