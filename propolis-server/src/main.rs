use anyhow::{anyhow, Result};
use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, HttpServerStarter, Path, RequestContext,
    TypedBody,
};
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::io::AsyncWriteExt;

use propolis::bhyve_api;
use propolis::block;
use propolis::chardev::{Sink, Source, UDSock};
use propolis::common::PAGE_SIZE;
use propolis::dispatch::Dispatcher;
use propolis::hw::chipset::{i440fx::I440Fx, Chipset};
use propolis::hw::ibmpc;
use propolis::hw::pci;
use propolis::hw::ps2ctrl::PS2Ctrl;
use propolis::hw::qemu::{debug::QemuDebugPort, fwcfg, ramfb};
use propolis::hw::uart::LpcUart;
use propolis::hw::virtio;
use propolis::instance::Instance;
use propolis::inventory::{EntityID, Inventory};
use propolis::vmm::{self, Builder, Machine, MachineCtx, Prot};

mod api;
mod config;

// TODO(error) Do a pass of HTTP codes (error and ok)
// TODO(idempotency) Idempotency mechanisms?
// TODO(refactor) Refactor "mechanism" code away from "server handling" code.

// All context for a single propolis instance.
struct InstanceContext {
    // The instance, which may or may not be instantiated.
    instance: Arc<Instance>,
    properties: api::InstanceProperties,
    serial: Serial,
}

struct ServerContext {
    context: tokio::sync::Mutex<Option<InstanceContext>>,
    config: config::Config,
}

impl ServerContext {
    fn new(config: config::Config) -> Self {
        ServerContext { context: tokio::sync::Mutex::new(None), config }
    }
}

// Represents the client side of the connection.
struct SerialConnection {
    stream: tokio::net::UnixStream,
}

impl SerialConnection {
    async fn new() -> std::io::Result<SerialConnection> {
        let stream = tokio::net::UnixStream::connect("./ttya").await?;
        Ok(SerialConnection { stream })
    }
}

// Represents a serial connection into the VM.
struct Serial {
    uart: Arc<LpcUart>,
    uds: Arc<UDSock>,
    conn: Option<SerialConnection>,
}

impl Serial {
    fn new(uart: Arc<LpcUart>, uds: Arc<UDSock>) -> std::io::Result<Serial> {
        Ok(Serial { uart, uds, conn: None })
    }

    async fn ensure_connected(&mut self) -> std::io::Result<()> {
        if let None = self.conn {
            println!("Connecting to serial: Autodiscard is now false");
            self.uart.source_set_autodiscard(false);
            self.conn = Some(SerialConnection::new().await?);
        } else {
            println!("Already connected");
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        self.uart.source_set_autodiscard(true);
        self.conn = None;
    }

    async fn read(&mut self) -> std::io::Result<Vec<u8>> {
        let conn = self.conn.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("not connected"),
            )
        })?;
        let mut buf = [0u8; 1 << 12];
        let n = match conn.stream.try_read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(e);
                }
                0
            }
        };
        println!("read {} bytes", n);
        Ok(buf[..n].to_vec())
    }

    async fn write(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let conn = self.conn.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("not connected"),
            )
        })?;
        conn.stream.write_all(&buf).await?;
        println!("write {} bytes", buf.len());
        Ok(())
    }
}

// TODO(refactor)
// Arbitrary ROM limit for now
const MAX_ROM_SIZE: usize = 0x20_0000;

fn open_bootrom<P: AsRef<std::path::Path>>(path: P) -> Result<(File, usize)> {
    let fp = File::open(path.as_ref())?;
    let len = fp.metadata()?.len();
    if len % (PAGE_SIZE as u64) != 0 {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "rom {} length {:x} not aligned to {:x}",
                path.as_ref().to_string_lossy(),
                len,
                PAGE_SIZE
            ),
        )
        .into())
    } else {
        Ok((fp, len as usize))
    }
}

// TODO(refactor)
fn build_instance(
    name: &str,
    max_cpu: u8,
    lowmem: usize,
) -> Result<Arc<Instance>> {
    let builder = Builder::new(name, true)?
        .max_cpus(max_cpu)?
        .add_mem_region(0, lowmem, Prot::ALL, "lowmem")?
        .add_rom_region(
            0x1_0000_0000 - MAX_ROM_SIZE,
            MAX_ROM_SIZE,
            Prot::READ | Prot::EXEC,
            "bootrom",
        )?
        .add_mmio_region(0xc000_0000_usize, 0x2000_0000_usize, "dev32")?
        .add_mmio_region(0xe000_0000_usize, 0x1000_0000_usize, "pcicfg")?
        .add_mmio_region(
            vmm::MAX_SYSMEM,
            vmm::MAX_PHYSMEM - vmm::MAX_SYSMEM,
            "dev64",
        )?;
    let inst = Instance::create(builder, propolis::vcpu_run_loop)?;
    Ok(inst)
}

struct RegisteredChipset(Arc<I440Fx>, EntityID);
impl RegisteredChipset {
    fn device(&self) -> &Arc<I440Fx> {
        &self.0
    }
    fn id(&self) -> EntityID {
        self.1
    }
}

struct MachineInitializer<'a> {
    machine: &'a Machine,
    mctx: &'a MachineCtx,
    inv: &'a Inventory,
    disp: &'a Dispatcher,
}

impl<'a> MachineInitializer<'a> {
    fn initialize_rom<P: AsRef<std::path::Path>>(
        &self,
        path: P,
    ) -> Result<(), Error> {
        let (romfp, rom_len) = open_bootrom(path.as_ref())
            .unwrap_or_else(|e| panic!("Cannot open bootrom: {}", e));
        self.machine.populate_rom("bootrom", |mapping| {
            let mapping = mapping.as_ref();
            if mapping.len() < rom_len {
                return Err(Error::new(ErrorKind::InvalidData, "rom too long"));
            }
            let offset = mapping.len() - rom_len;
            let submapping = mapping.subregion(offset, rom_len).unwrap();
            let nread = submapping.pread(&romfp, rom_len, 0)?;
            if nread != rom_len {
                // TODO: Handle short read
                return Err(Error::new(ErrorKind::InvalidData, "short read"));
            }
            Ok(())
        })?;
        Ok(())
    }

    fn initialize_chipset(&self) -> Result<RegisteredChipset, Error> {
        let hdl = self.machine.get_hdl();
        let chipset = I440Fx::create(Arc::clone(&hdl));
        chipset.attach(self.mctx);
        let id = self
            .inv
            .register_root(chipset.clone(), "chipset".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(RegisteredChipset(chipset, id))
    }

    fn initialize_uart(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<Serial, Error> {
        let com1_sock = UDSock::bind(std::path::Path::new("./ttya"))
            .unwrap_or_else(|e| panic!("Cannot bind UDSock: {}", e));

        // UARTs
        let com1 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM1).unwrap());
        let com2 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM2).unwrap());
        let com3 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM3).unwrap());
        let com4 =
            LpcUart::new(chipset.device().irq_pin(ibmpc::IRQ_COM4).unwrap());

        let ctx = self.disp.ctx();
        com1_sock.listen(&ctx);
        com1_sock.attach_sink(Arc::clone(&com1) as Arc<dyn Sink>);
        com1_sock.attach_source(Arc::clone(&com1) as Arc<dyn Source>);
        com1.source_set_autodiscard(true);

        // XXX: plumb up com2-4, but until then, just auto-discard
        com2.source_set_autodiscard(true);
        com3.source_set_autodiscard(true);
        com4.source_set_autodiscard(true);

        let pio = self.mctx.pio();
        LpcUart::attach(&com1, pio, ibmpc::PORT_COM1);
        LpcUart::attach(&com2, pio, ibmpc::PORT_COM2);
        LpcUart::attach(&com3, pio, ibmpc::PORT_COM3);
        LpcUart::attach(&com4, pio, ibmpc::PORT_COM4);
        self.inv
            .register(chipset.id(), com1.clone(), "com1".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), com2, "com2".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), com3, "com3".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), com4, "com4".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        Serial::new(com1, com1_sock)
    }

    fn initialize_ps2(&self, chipset: &RegisteredChipset) -> Result<(), Error> {
        let pio = self.mctx.pio();
        let ps2_ctrl = PS2Ctrl::create();
        ps2_ctrl.attach(pio, chipset.device().as_ref());
        self.inv
            .register(chipset.id(), ps2_ctrl, "ps2_ctrl".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    fn initialize_qemu_debug_port(
        &self,
        chipset: &RegisteredChipset,
    ) -> Result<(), Error> {
        let debug = std::fs::File::create("debug.out").unwrap();
        let buffered = std::io::LineWriter::new(debug);
        let pio = self.mctx.pio();
        let dbg = QemuDebugPort::create(
            Some(Box::new(buffered) as Box<dyn std::io::Write + Send>),
            pio,
        );
        self.inv
            .register(chipset.id(), dbg, "debug".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        Ok(())
    }

    // TODO: Add to block/vnic inventory?

    fn initialize_block<P: AsRef<std::path::Path>>(
        &self,
        chipset: &RegisteredChipset,
        path: P,
        bdf: pci::Bdf,
    ) -> Result<(), Error> {
        let plain = block::PlainBdev::create(path.as_ref())?;

        let vioblk = virtio::VirtioBlock::create(
            0x100,
            Arc::clone(&plain)
                as Arc<dyn block::BlockDev<virtio::block::Request>>,
        );
        chipset.device().pci_attach(bdf, vioblk);

        plain.start_dispatch(
            format!("bdev-{} thread", path.as_ref().to_string_lossy()),
            &self.disp,
        );
        Ok(())
    }

    fn initialize_vnic(
        &self,
        chipset: &RegisteredChipset,
        vnic_name: &str,
        bdf: pci::Bdf,
    ) -> Result<(), Error> {
        let hdl = self.machine.get_hdl();
        let viona = virtio::viona::VirtioViona::create(vnic_name, 0x100, &hdl)?;
        chipset.device().pci_attach(bdf, viona);
        Ok(())
    }

    fn initialize_fwcfg(
        &self,
        chipset: &RegisteredChipset,
        cpus: u8,
    ) -> Result<(), Error> {
        let mut fwcfg = fwcfg::FwCfgBuilder::new();
        fwcfg
            .add_legacy(
                fwcfg::LegacyId::SmpCpuCount,
                fwcfg::FixedItem::new_u32(cpus as u32),
            )
            .unwrap();

        let ramfb = ramfb::RamFb::create();
        ramfb.attach(&mut fwcfg);

        let fwcfg_dev = fwcfg.finalize();
        let pio = self.mctx.pio();
        fwcfg_dev.attach(pio);

        self.inv
            .register(chipset.id(), fwcfg_dev, "fwcfg".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;
        self.inv
            .register(chipset.id(), ramfb, "ramfb".to_string())
            .map_err(|e| -> std::io::Error { e.into() })?;

        let ncpu = self.mctx.max_cpus();
        for id in 0..ncpu {
            let mut vcpu = self.machine.vcpu(id);
            vcpu.set_default_capabs().unwrap();
            vcpu.reboot_state().unwrap();
            vcpu.activate().unwrap();
            // Set BSP to start up
            if id == 0 {
                vcpu.set_run_state(bhyve_api::VRS_RUN).unwrap();
                vcpu.set_reg(bhyve_api::vm_reg_name::VM_REG_GUEST_RIP, 0xfff0)
                    .unwrap();
            }
        }
        Ok(())
    }
}

fn api_to_propolis_state(
    state: api::InstanceStateRequested,
) -> propolis::instance::State {
    use api::InstanceStateRequested as ApiState;
    use propolis::instance::State as PropolisState;

    match state {
        ApiState::Run => PropolisState::Run,
        ApiState::Stop => PropolisState::Halt,
        ApiState::Reboot => PropolisState::Reset,
    }
}

fn propolis_to_api_state(
    state: propolis::instance::State,
) -> api::InstanceState {
    use api::InstanceState as ApiState;
    use propolis::instance::State as PropolisState;

    match state {
        PropolisState::Initialize => ApiState::Creating,
        PropolisState::Boot => ApiState::Starting,
        PropolisState::Run => ApiState::Running,
        PropolisState::Quiesce => ApiState::Stopped,
        PropolisState::Halt => todo!(),
        PropolisState::Reset => todo!(),
        PropolisState::Destroy => ApiState::Destroyed,
    }
}

/*
 * Instances: CRUD API
 */

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}",
}]
async fn instance_ensure(
    rqctx: Arc<RequestContext<ServerContext>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    println!("propolis-server: Instance Ensure");
    let server_context = rqctx.context();

    // TODO(idempotency) - how?

    let mut context = server_context.context.lock().await;

    if context.is_some() {
        return Err(HttpError::for_internal_error(
            "Server already initialized".to_string(),
        ));
    }

    let properties = request.into_inner().properties;
    if path_params.into_inner().instance_id != properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }

    // Create the instance.
    let lowmem = (properties.memory * 1024 * 1024) as usize;
    let instance = build_instance(&properties.name, properties.vcpus, lowmem)
        .map_err(|err| {
        HttpError::for_internal_error(format!(
            "Cannot build instance: {}",
            err.to_string()
        ))
    })?;

    // Initialize (some) of the instance's hardware.
    //
    // This initialization may be refactored to be client-controlled,
    // but it is currently hard-coded for simplicity.
    let mut com1: Option<Serial> = None;

    instance
        .initialize(|machine, mctx, disp, inv| {
            let init = MachineInitializer { machine, mctx, disp, inv };
            init.initialize_rom(server_context.config.get_bootrom())?;
            machine.initialize_rtc(lowmem).unwrap();
            let chipset = init.initialize_chipset()?;
            com1 = Some(init.initialize_uart(&chipset)?);
            init.initialize_ps2(&chipset)?;
            init.initialize_qemu_debug_port(&chipset)?;

            // Attach devices which are hard-coded in the config.
            //
            // NOTE: This interface is effectively a stop-gap for development
            // purposes. Longer term, peripherals will be attached via separate
            // HTTP interfaces.
            for (_, dev) in server_context.config.devs() {
                let driver = &dev.driver as &str;
                match driver {
                    "pci-virtio-block" => {
                        let path = dev.get_string("disk").ok_or_else(|| {
                            Error::new(
                                ErrorKind::InvalidData,
                                "Cannot parse disk path",
                            )
                        })?;
                        let bdf: pci::Bdf =
                            dev.get("pci-path").ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    "Cannot parse disk PCI",
                                )
                            })?;
                        init.initialize_block(&chipset, path, bdf)?;
                    }
                    "pci-virtio-viona" => {
                        let name = dev.get_string("vnic").ok_or_else(|| {
                            Error::new(
                                ErrorKind::InvalidData,
                                "Cannot parse vnic name",
                            )
                        })?;
                        let bdf: pci::Bdf =
                            dev.get("pci-path").ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    "Cannot parse vnic PCI",
                                )
                            })?;
                        init.initialize_vnic(&chipset, name, bdf)?;
                    }
                    _ => {
                        return Err(Error::new(
                            ErrorKind::InvalidData,
                            format!("Unknown driver in config: {}", driver),
                        ));
                    }
                }
            }

            // Finalize device.
            //
            // TODO: Can any of this happen *before* we attach devices?
            // TODO: How long do these steps take?
            chipset.device().pci_finalize(&disp.ctx());
            init.initialize_fwcfg(&chipset, properties.vcpus)?;
            Ok(())
        })
        .map_err(|err| {
            HttpError::for_internal_error(format!(
                "Failed to initialize machine: {}",
                err.to_string()
            ))
        })?;

    instance.print();
    instance.on_transition(Box::new(|next_state| {
        println!("state cb: {:?}", next_state);
    }));

    // Save the newly created instance in the server's context.
    *context =
        Some(InstanceContext { instance, properties, serial: com1.unwrap() });

    Ok(HttpResponseCreated(api::InstanceEnsureResponse {}))
}

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<ServerContext>>,
    path_params: Path<api::InstancePathParams>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    let context = rqctx.context().context.lock().await;

    let context = context.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

    if path_params.into_inner().instance_id != context.properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }
    let instance_info = api::Instance {
        properties: context.properties.clone(),
        state: propolis_to_api_state(context.instance.current_state()),

        // TODO(scaffolding)
        disks: vec![],
        nics: vec![],
    };

    Ok(HttpResponseOk(api::InstanceGetResponse { instance: instance_info }))
}

// TODO: Instance delete. What happens to the server? Does it shut down?

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/state",
}]
async fn instance_state_put(
    rqctx: Arc<RequestContext<ServerContext>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceStateRequested>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    println!("propolis-server: Instance State Put");
    let context = rqctx.context().context.lock().await;

    let context = context.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

    if path_params.into_inner().instance_id != context.properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }

    let state = api_to_propolis_state(request.into_inner());
    context.instance.set_target_state(state).map_err(|err| {
        HttpError::for_internal_error(format!("Failed to set state: {}", err))
    })?;

    Ok(HttpResponseUpdatedNoContent {})
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<ServerContext>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceSerialRequest>,
) -> Result<HttpResponseOk<api::InstanceSerialResponse>, HttpError> {
    let mut context = rqctx.context().context.lock().await;

    let context = context.as_mut().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;
    if path_params.into_inner().instance_id != context.properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }

    context.serial.ensure_connected().await.map_err(|e| {
        HttpError::for_internal_error(format!(
            "Cannot connect to serial: {}",
            e
        ))
    })?;
    let output = context.serial.read().await.map_err(|e| {
        HttpError::for_internal_error(format!("Cannot read from serial: {}", e))
    })?;
    context.serial.write(&request.into_inner().bytes).await.map_err(|e| {
        HttpError::for_internal_error(format!("Cannot write to serial: {}", e))
    })?;

    let response = api::InstanceSerialResponse { bytes: output };
    Ok(HttpResponseOk(response))
}

#[derive(Debug, StructOpt)]
#[structopt(
    name = "propolis-server",
    about = "An HTTP server providing access to Propolis"
)]
struct Opt {
    #[structopt(parse(from_os_str))]
    cfg: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Command line arguments.
    let opt = Opt::from_args();
    let config = config::parse(&opt.cfg)?;

    // Dropshot configuration.
    let config_dropshot: ConfigDropshot = Default::default();
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("propolis-server")
        .map_err(|error| anyhow!("failed to create logger: {}", error))?;

    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();

    let context = ServerContext::new(config);
    let server = HttpServerStarter::new(&config_dropshot, api, context, &log)
        .map_err(|error| anyhow!("Failed to start server: {}", error))?
        .start();
    server.await.map_err(|e| anyhow!("Server exited with an error: {}", e))
}
