// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::convert::TryInto;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::num::NonZeroUsize;
use std::os::unix::fs::FileTypeExt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::serial::Serial;
use crate::server::{BlockBackendMap, CrucibleBackendMap, DeviceMap};
use anyhow::{Context, Result};
use crucible_client_types::VolumeConstructionRequest;
pub use nexus_client::Client as NexusClient;
use oximeter::types::ProducerRegistry;
use propolis::block;
use propolis::chardev::{self, BlockingSource, Source};
use propolis::common::{Lifecycle, PAGE_SIZE};
use propolis::hw::bhyve::BhyveHpet;
use propolis::hw::chipset::{i440fx, Chipset};
use propolis::hw::ibmpc;
use propolis::hw::pci;
use propolis::hw::ps2::ctrl::PS2Ctrl;
use propolis::hw::qemu::pvpanic::QemuPvpanic;
use propolis::hw::qemu::{debug::QemuDebugPort, fwcfg, ramfb};
use propolis::hw::uart::LpcUart;
use propolis::hw::{nvme, virtio};
use propolis::intr_pins;
use propolis::vmm::{self, Builder, Machine};
use propolis_api_types::instance_spec::{self, v0::InstanceSpecV0};
use slog::info;

// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

fn get_spec_guest_ram_limits(spec: &InstanceSpecV0) -> (usize, usize) {
    const MB: usize = 1024 * 1024;
    const GB: usize = 1024 * 1024 * 1024;
    let memsize = spec.devices.board.memory_mb as usize * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);
    (lowmem, highmem)
}

pub fn build_instance(
    name: &str,
    spec: &InstanceSpecV0,
    use_reservoir: bool,
    _log: slog::Logger,
) -> Result<Machine> {
    let (lowmem, highmem) = get_spec_guest_ram_limits(spec);
    let create_opts = propolis::vmm::CreateOpts {
        force: true,
        use_reservoir,
        track_dirty: true,
    };
    let mut builder = Builder::new(name, create_opts)?
        .max_cpus(spec.devices.board.cpus)?
        .add_mem_region(0, lowmem, "lowmem")?
        .add_rom_region(0x1_0000_0000 - MAX_ROM_SIZE, MAX_ROM_SIZE, "bootrom")?
        .add_mmio_region(0xc000_0000_usize, 0x2000_0000_usize, "dev32")?
        .add_mmio_region(0xe000_0000_usize, 0x1000_0000_usize, "pcicfg")?;

    let highmem_start = 0x1_0000_0000;
    if highmem > 0 {
        builder = builder.add_mem_region(highmem_start, highmem, "highmem")?;
    }

    let dev64_start = highmem_start + highmem;
    builder = builder.add_mmio_region(
        dev64_start,
        vmm::MAX_PHYSMEM - dev64_start,
        "dev64",
    )?;

    Ok(builder.finalize()?)
}

pub struct RegisteredChipset {
    chipset: Arc<dyn Chipset>,
    isa: Arc<i440fx::Piix3Lpc>,
}
impl RegisteredChipset {
    pub fn pci_attach(&self, bdf: pci::Bdf, dev: Arc<dyn pci::Endpoint>) {
        self.chipset.pci_attach(bdf, dev, self.isa.route_lintr(bdf));
    }
    pub fn irq_pin(&self, irq: u8) -> Option<Box<dyn intr_pins::IntrPin>> {
        self.isa.irq_pin(irq)
    }
    fn reset_pin(&self) -> Arc<dyn intr_pins::IntrPin> {
        self.chipset.reset_pin()
    }
}

struct StorageBackendInstance {
    be: Arc<dyn block::Backend>,
    crucible: Option<(uuid::Uuid, Arc<block::CrucibleBackend>)>,
}

pub struct MachineInitializer<'a> {
    pub(crate) log: slog::Logger,
    pub(crate) machine: &'a Machine,
    pub(crate) devices: DeviceMap,
    pub(crate) block_backends: BlockBackendMap,
    pub(crate) crucible_backends: CrucibleBackendMap,
    pub(crate) spec: &'a InstanceSpecV0,
    pub(crate) producer_registry: Option<ProducerRegistry>,
}

impl<'a> MachineInitializer<'a> {
    pub fn initialize_rom(
        &mut self,
        path: &std::path::Path,
    ) -> Result<(), Error> {
        fn open_bootrom(path: &std::path::Path) -> Result<(File, usize)> {
            let fp = File::open(path)?;
            let len = fp.metadata()?.len();
            if len % (PAGE_SIZE as u64) != 0 {
                Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "rom {} length {:x} not aligned to {:x}",
                        path.to_string_lossy(),
                        len,
                        PAGE_SIZE
                    ),
                )
                .into())
            } else {
                Ok((fp, len as usize))
            }
        }

        let (romfp, rom_len) = open_bootrom(path)
            .unwrap_or_else(|e| panic!("Cannot open bootrom: {}", e));

        let mem = self.machine.acc_mem.access().unwrap();
        let mapping = mem.direct_writable_region_by_name("bootrom")?;
        let offset = mapping.len() - rom_len;
        let submapping = mapping.subregion(offset, rom_len).unwrap();
        let nread = submapping.pread(&romfp, rom_len, 0)?;
        if nread != rom_len {
            // TODO: Handle short read
            return Err(Error::new(ErrorKind::InvalidData, "short read"));
        }
        Ok(())
    }

    pub fn initialize_rtc(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        let (lowmem, highmem) = get_spec_guest_ram_limits(self.spec);

        let rtc = chipset.isa.rtc.as_ref();
        rtc.memsize_to_nvram(lowmem as u32, highmem as u64)?;
        rtc.set_time(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time precedes UNIX epoch"),
        )?;

        Ok(())
    }

    pub fn initialize_hpet(&mut self) -> Result<(), Error> {
        let hpet = BhyveHpet::create(self.machine.hdl.clone());
        self.devices.insert(hpet.type_name().into(), hpet.clone());
        Ok(())
    }

    pub fn initialize_chipset(
        &mut self,
        event_handler: &Arc<dyn super::vm::ChipsetEventHandler>,
    ) -> Result<RegisteredChipset, Error> {
        let mut pci_builder = pci::topology::Builder::new();
        for (name, bridge) in &self.spec.devices.pci_pci_bridges {
            let desc = pci::topology::BridgeDescription::new(
                pci::topology::LogicalBusId(bridge.downstream_bus),
                bridge.pci_path.try_into().map_err(|e| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Couldn't get PCI BDF for bridge {}: {}",
                            name, e
                        ),
                    )
                })?,
            );
            pci_builder.add_bridge(desc)?;
        }
        let pci::topology::FinishedTopology { topology: pci_topology, bridges } =
            pci_builder.finish(self.machine)?;

        match self.spec.devices.board.chipset {
            instance_spec::components::board::Chipset::I440Fx(i440fx) => {
                let power_ref = Arc::downgrade(event_handler);
                let reset_ref = Arc::downgrade(event_handler);
                let power_pin = Arc::new(propolis::intr_pins::FuncPin::new(
                    Box::new(move |rising| {
                        if rising {
                            if let Some(handler) = power_ref.upgrade() {
                                handler.chipset_halt();
                            }
                        }
                    }),
                ));
                let reset_pin = Arc::new(propolis::intr_pins::FuncPin::new(
                    Box::new(move |rising| {
                        if rising {
                            if let Some(handler) = reset_ref.upgrade() {
                                handler.chipset_reset();
                            }
                        }
                    }),
                ));

                let chipset_hb = i440fx::I440FxHostBridge::create(
                    pci_topology,
                    i440fx::Opts {
                        power_pin: Some(power_pin),
                        reset_pin: Some(reset_pin),
                        enable_pcie: i440fx.enable_pcie,
                    },
                );
                let chipset_lpc =
                    i440fx::Piix3Lpc::create(self.machine.hdl.clone());

                let chipset_pm = i440fx::Piix3PM::create(
                    self.machine.hdl.clone(),
                    chipset_hb.power_pin(),
                    self.log.new(slog::o!("device" => "piix3pm")),
                );

                let do_pci_attach = |bdf, dev: Arc<dyn pci::Endpoint>| {
                    chipset_hb.pci_attach(
                        bdf,
                        dev,
                        chipset_lpc.route_lintr(bdf),
                    );
                };

                // Attach chipset devices to PCI and buses
                do_pci_attach(i440fx::DEFAULT_HB_BDF, chipset_hb.clone());
                chipset_hb.attach(self.machine);

                do_pci_attach(i440fx::DEFAULT_LPC_BDF, chipset_lpc.clone());
                chipset_lpc.attach(&self.machine.bus_pio);

                do_pci_attach(i440fx::DEFAULT_PM_BDF, chipset_pm.clone());
                chipset_pm.attach(&self.machine.bus_pio);

                self.devices
                    .insert(chipset_hb.type_name().into(), chipset_hb.clone());
                self.devices.insert(
                    chipset_lpc.type_name().into(),
                    chipset_lpc.clone(),
                );
                self.devices.insert(chipset_pm.type_name().into(), chipset_pm);

                // Record attachment for any bridges in PCI topology too
                for (bdf, bridge) in bridges {
                    self.devices.insert(
                        format!("{}-{bdf}", bridge.type_name()),
                        bridge,
                    );
                }

                Ok(RegisteredChipset { chipset: chipset_hb, isa: chipset_lpc })
            }
        }
    }

    pub fn initialize_uart(
        &mut self,
        chipset: &RegisteredChipset,
    ) -> Result<Serial<LpcUart>, Error> {
        use instance_spec::components::devices::SerialPortNumber;

        let mut com1 = None;
        for (name, serial_spec) in &self.spec.devices.serial_ports {
            let (irq, port) = match serial_spec.num {
                SerialPortNumber::Com1 => (ibmpc::IRQ_COM1, ibmpc::PORT_COM1),
                SerialPortNumber::Com2 => (ibmpc::IRQ_COM2, ibmpc::PORT_COM2),
                SerialPortNumber::Com3 => (ibmpc::IRQ_COM3, ibmpc::PORT_COM3),
                SerialPortNumber::Com4 => (ibmpc::IRQ_COM4, ibmpc::PORT_COM4),
            };

            let dev = LpcUart::new(chipset.irq_pin(irq).unwrap());
            dev.set_autodiscard(true);
            LpcUart::attach(&dev, &self.machine.bus_pio, port);
            self.devices.insert(name.clone(), dev.clone());
            if matches!(serial_spec.num, SerialPortNumber::Com1) {
                assert!(com1.is_none());
                com1 = Some(dev);
            }
        }

        let sink_size = NonZeroUsize::new(64).unwrap();
        let source_size = NonZeroUsize::new(1024).unwrap();
        Ok(Serial::new(com1.unwrap(), sink_size, source_size))
    }

    pub fn initialize_ps2(
        &mut self,
        chipset: &RegisteredChipset,
    ) -> Result<Arc<PS2Ctrl>, Error> {
        let ps2_ctrl = PS2Ctrl::create();

        ps2_ctrl.attach(
            &self.machine.bus_pio,
            chipset.irq_pin(ibmpc::IRQ_PS2_PRI).unwrap(),
            chipset.irq_pin(ibmpc::IRQ_PS2_AUX).unwrap(),
            chipset.reset_pin(),
        );
        self.devices.insert(ps2_ctrl.type_name().into(), ps2_ctrl.clone());

        Ok(ps2_ctrl)
    }

    pub fn initialize_qemu_debug_port(&mut self) -> Result<(), Error> {
        let dbg = QemuDebugPort::create(&self.machine.bus_pio);
        let debug_file = std::fs::File::create("debug.out")?;
        let poller = chardev::BlockingFileOutput::new(debug_file);

        poller.attach(Arc::clone(&dbg) as Arc<dyn BlockingSource>);
        self.devices.insert(dbg.type_name().into(), dbg);

        Ok(())
    }

    pub fn initialize_qemu_pvpanic(
        &mut self,
        uuid: uuid::Uuid,
    ) -> Result<(), anyhow::Error> {
        if let Some(ref spec) = self.spec.devices.qemu_pvpanic {
            if spec.enable_isa {
                let pvpanic = QemuPvpanic::create(
                    self.log.new(slog::o!("dev" => "qemu-pvpanic")),
                );
                pvpanic.attach_pio(&self.machine.bus_pio);
                self.devices
                    .insert(pvpanic.type_name().into(), pvpanic.clone());

                if let Some(ref registry) = self.producer_registry {
                    let producer =
                        crate::stats::PvpanicProducer::new(uuid, pvpanic);
                    registry.register_producer(producer).context(
                        "failed to register PVPANIC Oximeter producer",
                    )?;
                }
            }
        }

        Ok(())
    }

    fn create_storage_backend_from_spec(
        &self,
        backend_spec: &instance_spec::v0::StorageBackendV0,
        backend_name: &str,
        nexus_client: &Option<NexusClient>,
    ) -> Result<StorageBackendInstance, Error> {
        match backend_spec {
            instance_spec::v0::StorageBackendV0::Crucible(spec) => {
                info!(self.log, "Creating Crucible disk";
                      "backend_name" => backend_name);

                let vcr: VolumeConstructionRequest =
                    serde_json::from_str(&spec.request_json)?;

                let cru_id = match vcr {
                    VolumeConstructionRequest::Volume { id, .. } => {
                        id.to_string()
                    }
                    VolumeConstructionRequest::File { id, .. } => {
                        id.to_string()
                    }
                    VolumeConstructionRequest::Url { id, .. } => id.to_string(),
                    VolumeConstructionRequest::Region { .. } => {
                        "Region".to_string()
                    }
                };

                let be = propolis::block::CrucibleBackend::create(
                    vcr,
                    propolis::block::BackendOpts {
                        read_only: Some(spec.readonly),
                        ..Default::default()
                    },
                    self.producer_registry.clone(),
                    nexus_client.clone(),
                    self.log.new(
                        slog::o!("component" => format!("crucible-{cru_id}")),
                    ),
                )?;

                let crucible = Some((be.get_uuid()?, be.clone()));
                Ok(StorageBackendInstance { be, crucible })
            }
            instance_spec::v0::StorageBackendV0::File(spec) => {
                info!(self.log, "Creating file disk backend";
                      "path" => &spec.path);

                // Check if raw device is being used and gripe if it isn't
                let meta = std::fs::metadata(&spec.path)?;
                if meta.file_type().is_block_device() {
                    slog::warn!(
                        self.log,
                        "Block backend using standard device rather than raw";
                        "path" => &spec.path
                    );
                }

                let nworkers = NonZeroUsize::new(8).unwrap();
                let be = propolis::block::FileBackend::create(
                    &spec.path,
                    propolis::block::BackendOpts {
                        read_only: Some(spec.readonly),
                        ..Default::default()
                    },
                    nworkers,
                )?;

                Ok(StorageBackendInstance { be, crucible: None })
            }
            instance_spec::v0::StorageBackendV0::Blob(spec) => {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &spec.base64,
                )
                .map_err(|e| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "failed to decode base64 contents of in-memory \
                                disk: {}",
                            e
                        ),
                    )
                })?;

                info!(self.log, "Creating in-memory disk backend";
                      "len" => bytes.len());

                let nworkers = NonZeroUsize::new(8).unwrap();
                let be = propolis::block::InMemoryBackend::create(
                    bytes,
                    propolis::block::BackendOpts {
                        block_size: Some(512),
                        read_only: Some(spec.readonly),
                        ..Default::default()
                    },
                    nworkers,
                )?;

                Ok(StorageBackendInstance { be, crucible: None })
            }
        }
    }

    /// Initializes the storage devices and backends listed in this
    /// initializer's instance spec.
    ///
    /// On success, returns a map from Crucible backend IDs to Crucible
    /// backends.
    pub fn initialize_storage_devices(
        &mut self,
        chipset: &RegisteredChipset,
        nexus_client: Option<NexusClient>,
    ) -> Result<(), Error> {
        enum DeviceInterface {
            Virtio,
            Nvme,
        }

        for (name, device_spec) in &self.spec.devices.storage_devices {
            info!(
                self.log,
                "Creating storage device {} with properties {:?}",
                name,
                device_spec
            );

            let (device_interface, backend_name, pci_path) = match device_spec {
                instance_spec::v0::StorageDeviceV0::VirtioDisk(disk) => {
                    (DeviceInterface::Virtio, &disk.backend_name, disk.pci_path)
                }
                instance_spec::v0::StorageDeviceV0::NvmeDisk(disk) => {
                    (DeviceInterface::Nvme, &disk.backend_name, disk.pci_path)
                }
            };

            let backend_spec = self
                .spec
                .backends
                .storage_backends
                .get(backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for storage device {}",
                            backend_name, name
                        ),
                    )
                })?;

            let bdf: pci::Bdf = pci_path.try_into().map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Couldn't get PCI BDF for storage device {}: {}",
                        name, e
                    ),
                )
            })?;

            let StorageBackendInstance { be: backend, crucible } = self
                .create_storage_backend_from_spec(
                    backend_spec,
                    backend_name,
                    &nexus_client,
                )?;

            self.block_backends.insert(backend_name.clone(), backend.clone());
            match device_interface {
                DeviceInterface::Virtio => {
                    let vioblk = virtio::PciVirtioBlock::new(0x100);

                    self.devices
                        .insert(format!("pci-virtio-{}", bdf), vioblk.clone());
                    block::attach(backend, vioblk.clone());
                    chipset.pci_attach(bdf, vioblk);
                }
                DeviceInterface::Nvme => {
                    let nvme = nvme::PciNvme::create(
                        name.to_string(),
                        self.log.new(
                            slog::o!("component" => format!("nvme-{}", name)),
                        ),
                    );
                    self.devices
                        .insert(format!("pci-nvme-{bdf}"), nvme.clone());
                    block::attach(backend, nvme.clone());
                    chipset.pci_attach(bdf, nvme);
                }
            };

            if let Some((id, backend)) = crucible {
                let prev = self.crucible_backends.insert(id, backend);
                if prev.is_some() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("multiple disks with id {}", id),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn initialize_network_devices(
        &mut self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        for (name, vnic_spec) in &self.spec.devices.network_devices {
            info!(self.log, "Creating vNIC {}", name);
            let instance_spec::v0::NetworkDeviceV0::VirtioNic(vnic_spec) =
                vnic_spec;

            let backend_spec = self
                .spec
                .backends
                .network_backends
                .get(&vnic_spec.backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for vNIC {}",
                            vnic_spec.backend_name, name
                        ),
                    )
                })?;
            let bdf: pci::Bdf = vnic_spec.pci_path.try_into().map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("Couldn't get PCI BDF for vNIC {}: {}", name, e),
                )
            })?;

            let vnic_name = match backend_spec {
                instance_spec::v0::NetworkBackendV0::Virtio(spec) => {
                    &spec.vnic_name
                }
                instance_spec::v0::NetworkBackendV0::Dlpi(_) => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Network backend must be virtio for vNIC {}",
                            name,
                        ),
                    ));
                }
            };

            let viona = virtio::PciVirtioViona::new(
                vnic_name,
                0x100,
                &self.machine.hdl,
            )?;
            self.devices
                .insert(format!("pci-virtio-viona-{}", bdf), viona.clone());
            chipset.pci_attach(bdf, viona);
        }
        Ok(())
    }

    #[cfg(not(feature = "omicron-build"))]
    pub fn initialize_test_devices(
        &mut self,
        toml_cfg: &std::collections::BTreeMap<
            String,
            propolis_server_config::Device,
        >,
    ) -> Result<(), Error> {
        use propolis::hw::testdev::{
            MigrationFailureDevice, MigrationFailures,
        };

        if let Some(dev) = toml_cfg.get(MigrationFailureDevice::NAME) {
            const FAIL_EXPORTS: &str = "fail_exports";
            const FAIL_IMPORTS: &str = "fail_imports";
            let fail_exports = dev
                .options
                .get(FAIL_EXPORTS)
                .and_then(|val| val.as_integer())
                .unwrap_or(0);
            let fail_imports = dev
                .options
                .get(FAIL_IMPORTS)
                .and_then(|val| val.as_integer())
                .unwrap_or(0);

            if fail_exports <= 0 && fail_imports <= 0 {
                info!(
                    self.log,
                    "migration failure device will not fail, as both
                    `{FAIL_EXPORTS}` and `{FAIL_IMPORTS}` are 0";
                    FAIL_EXPORTS => ?fail_exports,
                    FAIL_IMPORTS => ?fail_imports,
                );
            }

            let dev = MigrationFailureDevice::create(
                &self.log,
                MigrationFailures {
                    exports: fail_exports as usize,
                    imports: fail_imports as usize,
                },
            );
            self.devices.insert(MigrationFailureDevice::NAME.into(), dev);
        }

        Ok(())
    }

    #[cfg(feature = "falcon")]
    pub fn initialize_softnpu_ports(
        &mut self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        // Check to make sure we actually have both a pci port and at least one
        // regular SoftNpu port, otherwise just return.
        let pci_port = match &self.spec.devices.softnpu_pci_port {
            Some(tfp) => tfp,
            None => return Ok(()),
        };
        if self.spec.devices.softnpu_ports.is_empty() {
            return Ok(());
        }

        let mut ports: Vec<&instance_spec::components::devices::SoftNpuPort> =
            self.spec.devices.softnpu_ports.values().collect();

        // SoftNpu ports are named <topology>_<node>_vnic<N> by falcon, where
        // <N> indicates the intended order.
        ports.sort_by_key(|p| &p.name);

        let mut data_links: Vec<String> = Vec::new();
        for x in &ports {
            let backend = self
                .spec
                .backends
                .network_backends
                .get(&x.backend_name)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Backend {} not found for softnpu port",
                            x.backend_name
                        ),
                    )
                })?;

            let vnic = match &backend {
                instance_spec::v0::NetworkBackendV0::Dlpi(dlpi) => {
                    &dlpi.vnic_name
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Softnpu port must have DLPI backend: {}",
                            x.backend_name
                        ),
                    ));
                }
            };
            data_links.push(vnic.clone());
        }

        // Set up an LPC uart for ASIC management comms from the guest.
        //
        // NOTE: SoftNpu squats on com4.
        let uart = LpcUart::new(chipset.irq_pin(ibmpc::IRQ_COM4).unwrap());
        uart.set_autodiscard(true);
        LpcUart::attach(&uart, &self.machine.bus_pio, ibmpc::PORT_COM4);
        self.devices.insert("softnpu-uart".to_string(), uart.clone());

        // Start with no pipeline. The guest must load the initial P4 program.
        let pipeline = Arc::new(std::sync::Mutex::new(None));

        // Set up the p9fs device for guest programs to load P4 programs
        // through.
        let p9_handler = virtio::softnpu::SoftNpuP9Handler::new(
            "/dev/softnpufs".to_owned(),
            "/dev/softnpufs".to_owned(),
            self.spec.devices.softnpu_ports.len() as u16,
            pipeline.clone(),
            self.log.clone(),
        );
        let vio9p =
            virtio::p9fs::PciVirtio9pfs::new(0x40, Arc::new(p9_handler));
        self.devices.insert("softnpu-p9fs".to_string(), vio9p.clone());
        let bdf: pci::Bdf = self
            .spec
            .devices
            .softnpu_p9
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "SoftNpu p9 device missing".to_owned(),
                )
            })?
            .pci_path
            .try_into()
            .map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Couldn't get PCI BDF for SoftNpu p9 device: {}",
                        e
                    ),
                )
            })?;
        chipset.pci_attach(bdf, vio9p.clone());

        // Create the SoftNpu device.
        let queue_size = 0x8000;
        let softnpu = virtio::softnpu::SoftNpu::new(
            data_links,
            queue_size,
            uart,
            vio9p,
            pipeline,
            self.log.clone(),
        )
        .map_err(|e| -> std::io::Error {
            let io_err: std::io::Error = e;
            std::io::Error::new(
                io_err.kind(),
                format!("register softnpu: {}", io_err),
            )
        })?;
        self.devices.insert("softnpu-main".to_string(), softnpu.clone());

        // Create the SoftNpu PCI port.
        let bdf: pci::Bdf = pci_port.pci_path.try_into().map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Couldn't get PCI BDF for SoftNpu pci port: {}", e),
            )
        })?;
        self.devices
            .insert("softnpu-pciport".to_string(), softnpu.pci_port.clone());
        chipset.pci_attach(bdf, softnpu.pci_port.clone());

        Ok(())
    }

    #[cfg(feature = "falcon")]
    pub fn initialize_9pfs(
        &mut self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        // Check that there is actually a p9fs device to register, if not bail
        // early.
        let p9fs = match &self.spec.devices.p9fs {
            Some(p9fs) => p9fs,
            None => return Ok(()),
        };

        let bdf: pci::Bdf = p9fs.pci_path.try_into().map_err(|e| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Couldn't get PCI BDF for p9fs device: {}", e),
            )
        })?;

        let handler = virtio::p9fs::HostFSHandler::new(
            p9fs.source.to_owned(),
            p9fs.target.to_owned(),
            p9fs.chunk_size,
            self.log.clone(),
        );
        let vio9p = virtio::p9fs::PciVirtio9pfs::new(0x40, Arc::new(handler));
        self.devices.insert("falcon-p9fs".to_string(), vio9p.clone());
        chipset.pci_attach(bdf, vio9p);
        Ok(())
    }

    pub fn initialize_fwcfg(
        &mut self,
        cpus: u8,
    ) -> Result<Arc<ramfb::RamFb>, Error> {
        let mut fwcfg = fwcfg::FwCfgBuilder::new();
        fwcfg
            .add_legacy(
                fwcfg::LegacyId::SmpCpuCount,
                fwcfg::FixedItem::new_u32(cpus as u32),
            )
            .unwrap();

        let ramfb = ramfb::RamFb::create(
            self.log.new(slog::o!("component" => "ramfb")),
        );
        ramfb.attach(&mut fwcfg, &self.machine.acc_mem);

        let fwcfg_dev = fwcfg.finalize();
        fwcfg_dev.attach(&self.machine.bus_pio, &self.machine.acc_mem);

        self.devices.insert(fwcfg_dev.type_name().into(), fwcfg_dev);
        self.devices.insert(ramfb.type_name().into(), ramfb.clone());
        Ok(ramfb)
    }

    pub fn initialize_cpus(&mut self) -> Result<(), Error> {
        for vcpu in self.machine.vcpus.iter() {
            vcpu.set_default_capabs().unwrap();

            // The vCPUs behave like devices, so add them to the list as well
            self.devices.insert(format!("vcpu-{}", vcpu.id), vcpu.clone());
        }
        Ok(())
    }
}
