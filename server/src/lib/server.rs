//! HTTP server callback functions.

use anyhow::Result;
use dropshot::{
    endpoint, ApiDescription, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, RequestContext, TypedBody,
};
use futures::future::Fuse;
use futures::{FutureExt, SinkExt, StreamExt};
use hyper::upgrade::{self, Upgraded};
use hyper::{header, Body, Response, StatusCode};
use propolis::hw::qemu::ramfb::RamFb;
use rfb::server::VncServer;
use slog::{error, info, o, Logger};
use std::borrow::Cow;
use std::io::{Error, ErrorKind};
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{
    self, handshake, protocol::Role, Message,
};
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use propolis::bhyve_api;
use propolis::dispatch::AsyncCtx;
use propolis::hw::pci;
use propolis::hw::uart::LpcUart;
use propolis::instance::Instance;
use propolis_client::api;

use crate::config::Config;
use crate::initializer::{build_instance, MachineInitializer};
use crate::serial::Serial;
use crate::vnc::PropolisVncServer;
use crate::{migrate, vnc};

// TODO(error) Do a pass of HTTP codes (error and ok)
// TODO(idempotency) Idempotency mechanisms?

/// Errors which may occur during the course of a serial connection
#[derive(Error, Debug)]
enum SerialTaskError {
    #[error("Cannot upgrade HTTP request to WebSockets: {0}")]
    Upgrade(#[from] hyper::Error),

    #[error("WebSocket Error: {0}")]
    WebSocket(#[from] tungstenite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

struct SerialTask {
    /// Handle to attached serial session
    task: JoinHandle<()>,
    /// Oneshot channel used to detach an attached serial session
    detach_ch: oneshot::Sender<()>,
}

impl SerialTask {
    /// Is the serial task still attached
    fn is_attached(&self) -> bool {
        // Use whether the detach channel has been closed as
        // a proxy for whether or not the task is still active
        !self.detach_ch.is_closed()
    }
}

#[derive(Clone)]
struct StateChange {
    gen: u64,
    state: propolis::instance::State,
}

// All context for a single propolis instance.
pub(crate) struct InstanceContext {
    // The instance, which may or may not be instantiated.
    pub instance: Arc<Instance>,
    pub properties: api::InstanceProperties,
    serial: Option<Arc<Serial<LpcUart>>>,
    state_watcher: watch::Receiver<StateChange>,
    serial_task: Option<SerialTask>,
}

/// Contextual information accessible from HTTP callbacks.
pub struct Context {
    pub(crate) context: Mutex<Option<InstanceContext>>,
    pub(crate) migrate_task: Mutex<Option<migrate::MigrateTask>>,
    config: Config,
    log: Logger,
    pub(crate) vnc_server: Arc<Mutex<VncServer<PropolisVncServer>>>,
}

impl Context {
    /// Creates a new server context object.
    pub fn new(
        config: Config,
        vnc_server: VncServer<PropolisVncServer>,
        log: Logger,
    ) -> Self {
        Context {
            context: Mutex::new(None),
            migrate_task: Mutex::new(None),
            config,
            log,
            vnc_server: Arc::new(Mutex::new(vnc_server)),
        }
    }
}

fn api_to_propolis_state(
    state: api::InstanceStateRequested,
) -> propolis::instance::ReqState {
    use api::InstanceStateRequested as ApiState;
    use propolis::instance::ReqState as PropolisState;

    match state {
        ApiState::Run => PropolisState::Run,
        ApiState::Stop => PropolisState::Halt,
        ApiState::Reboot => PropolisState::Reset,
        ApiState::MigrateStart => PropolisState::StartMigrate,
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
        PropolisState::Quiesce => ApiState::Stopping,
        PropolisState::Migrate(_, _) => ApiState::Migrating,
        PropolisState::Halt => ApiState::Stopped,
        PropolisState::Reset => ApiState::Rebooting,
        PropolisState::Destroy => ApiState::Destroyed,
    }
}

#[derive(Clone, Copy, Debug)]
enum SlotType {
    NIC,
    Disk,
    CloudInit,
}

// TODO: Slot ranges as constants, exposed to Omicron?

// This is a somewhat hard-coded translation of a stable "PCI slot" to a BDF.
//
// For all the devices requested by Nexus (network interfaces, disks, etc),
// we'd like to assign a stable PCI slot, such that re-allocating these
// devices on a new instance of propolis produces the same guest-visible
// BDFs.
fn slot_to_bdf(slot: api::Slot, ty: SlotType) -> Result<pci::Bdf> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::NIC if slot.0 <= 7 => {
            Ok(pci::Bdf::new(0, slot.0 + 0x8, 0).unwrap())
        }
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => {
            Ok(pci::Bdf::new(0, slot.0 + 0x10, 0).unwrap())
        }
        // Slot for CloudInit
        SlotType::CloudInit if slot.0 == 0 => {
            Ok(pci::Bdf::new(0, slot.0 + 0x18, 0).unwrap())
        }
        _ => Err(anyhow::anyhow!(
            "PCI Slot {} has no translation to BDF for type {:?}",
            slot.0,
            ty
        )),
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
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    let server_context = rqctx.context();

    let request = request.into_inner();
    let instance_id = path_params.into_inner().instance_id;
    let (properties, nics, disks, cloud_init_bytes) = (
        request.properties,
        request.nics,
        request.disks,
        request.cloud_init_bytes,
    );
    if instance_id != properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }

    // Handle requests to an instance that has already been initialized.
    let mut context = server_context.context.lock().await;
    if let Some(ctx) = &*context {
        if ctx.properties.id != properties.id {
            return Err(HttpError::for_internal_error(format!(
                "Server already initialized with ID {}",
                ctx.properties.id
            )));
        }

        // If properties match, we return Ok. Otherwise, we could attempt to
        // update the instance.
        //
        // TODO: The initial implementation does not modify any properties,
        // but we plausibly could do so - need to work out which properties
        // can be changed without rebooting.
        //
        // TODO: We presumably would want to alter network interfaces here too.
        // Might require the instance to be powered off.
        if ctx.properties != properties {
            return Err(HttpError::for_internal_error(
                "Cannot update running server".to_string(),
            ));
        }

        return Ok(HttpResponseCreated(api::InstanceEnsureResponse {
            migrate: None,
        }));
    }

    const MB: usize = 1024 * 1024;
    const GB: usize = 1024 * 1024 * 1024;
    let memsize = properties.memory as usize * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);

    // Create child logger for instance-related messages
    let vmm_log = server_context.log.new(o!("component" => "vmm"));

    // Create the instance.
    //
    // The VM is named after the UUID, ensuring that it is unique.
    let instance = build_instance(
        &properties.id.to_string(),
        properties.vcpus,
        lowmem,
        highmem,
        vmm_log,
    )
    .map_err(|err| {
        HttpError::for_internal_error(format!("Cannot build instance: {}", err))
    })?;

    let (tx, rx) = watch::channel(StateChange {
        gen: 0,
        state: propolis::instance::State::Initialize,
    });

    instance.on_transition(Box::new(move |next_state, _inv, _ctx| {
        let last = (*tx.borrow()).clone();
        let _ = tx.send(StateChange { gen: last.gen + 1, state: next_state });
    }));

    let mut com1 = None;
    let mut ramfb: Option<Arc<RamFb>> = None;
    let mut rt_handle = None;

    // Initialize (some) of the instance's hardware.
    //
    // This initialization may be refactored to be client-controlled,
    // but it is currently hard-coded for simplicity.

    instance
        .initialize(|machine, mctx, disp, inv| {
            let init = MachineInitializer::new(
                rqctx.log.clone(),
                machine,
                mctx,
                disp,
                inv,
            );
            init.initialize_rom(server_context.config.get_bootrom())?;
            init.initialize_kernel_devs(lowmem, highmem)?;
            let chipset = init.initialize_chipset()?;
            com1 = Some(Arc::new(init.initialize_uart(&chipset)?));
            init.initialize_ps2(&chipset)?;
            init.initialize_qemu_debug_port()?;

            // Attach devices which have been requested from the HTTP interface.
            for nic in &nics {
                info!(rqctx.log, "Creating NIC: {:#?}", nic);
                let bdf =
                    slot_to_bdf(nic.slot, SlotType::NIC).map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidData,
                            format!("Cannot parse vnic PCI: {}", e),
                        )
                    })?;
                init.initialize_vnic(&chipset, &nic.name, bdf)?;
            }

            for disk in &disks {
                info!(rqctx.log, "Creating Disk: {:#?}", disk);
                let bdf =
                    slot_to_bdf(disk.slot, SlotType::Disk).map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidData,
                            format!("Cannot parse disk PCI: {}", e),
                        )
                    })?;

                init.initialize_crucible(&chipset, disk, bdf)?;
                info!(rqctx.log, "Disk {} created successfully", disk.name);
            }

            if let Some(cloud_init_bytes) = &cloud_init_bytes {
                info!(rqctx.log, "Creating cloud-init disk");
                let bdf = slot_to_bdf(api::Slot(0), SlotType::CloudInit)
                    .map_err(|e| {
                        Error::new(ErrorKind::InvalidData, e.to_string())
                    })?;

                let bytes = base64::decode(&cloud_init_bytes).map_err(|e| {
                    Error::new(ErrorKind::InvalidInput, e.to_string())
                })?;

                init.initialize_in_memory_virtio_from_bytes(
                    &chipset,
                    "cloud-init",
                    bytes,
                    bdf,
                    true,
                )?;

                info!(rqctx.log, "cloud-init disk created");
            }

            // Attach devices which are hard-coded in the config.
            //
            // NOTE: This interface is effectively a stop-gap for development
            // purposes. Longer term, peripherals will be attached via separate
            // HTTP interfaces.
            for (devname, dev) in server_context.config.devs() {
                let driver = &dev.driver as &str;
                match driver {
                    "pci-virtio-block" => {
                        let block_dev_name = dev
                            .options
                            .get("block_dev")
                            .ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    format!(
                                        "no block_dev key for {}!",
                                        devname
                                    ),
                                )
                            })?
                            .as_str()
                            .ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    format!(
                                        "as_str() failed for {}'s block_dev!",
                                        devname
                                    ),
                                )
                            })?;

                        let (backend, creg) = server_context
                            .config
                            .create_block_backend(block_dev_name, &disp)
                            .map_err(|e| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    format!("ParseError: {:?}", e),
                                )
                            })?;

                        let bdf: pci::Bdf =
                            dev.get("pci-path").ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    "Cannot parse disk PCI",
                                )
                            })?;

                        init.initialize_virtio_block(
                            &chipset, bdf, backend, creg,
                        )?;
                    }
                    "pci-nvme" => {
                        let block_dev_name = dev
                            .options
                            .get("block_dev")
                            .ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    format!(
                                        "no block_dev key for {}!",
                                        devname
                                    ),
                                )
                            })?
                            .as_str()
                            .ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    format!(
                                        "as_str() failed for {}'s block_dev!",
                                        devname
                                    ),
                                )
                            })?;

                        let (backend, creg) = server_context
                            .config
                            .create_block_backend(block_dev_name, &disp)
                            .map_err(|e| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    format!("ParseError: {:?}", e),
                                )
                            })?;

                        let bdf: pci::Bdf =
                            dev.get("pci-path").ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    "Cannot parse disk PCI",
                                )
                            })?;

                        init.initialize_nvme_block(
                            &chipset,
                            bdf,
                            block_dev_name.to_string(),
                            backend,
                            creg,
                        )?;
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

            let ramfb_id = init.initialize_fwcfg(properties.vcpus)?;
            ramfb = inv.get_concrete(ramfb_id);
            rt_handle = disp.handle();
            init.initialize_cpus()?;
            Ok(())
        })
        .map_err(|err| {
            HttpError::for_internal_error(format!(
                "Failed to initialize machine: {}",
                err
            ))
        })?;

    // Initialize framebuffer data for the VNC server.
    let vnc_hdl = Arc::clone(&server_context.vnc_server);
    let (addr, width, height) = ramfb.as_ref().unwrap().get_framebuffer_info();
    let fb = vnc::RamFb::new(addr, width, height);
    let actx = instance.async_ctx();
    let vnc_server = vnc_hdl.lock().await;
    vnc_server.server.set_async_ctx(actx).await;
    vnc_server.server.initialize_framebuffer(fb).await;

    let rt = rt_handle.unwrap().clone();
    let hdl = Arc::clone(&vnc_hdl);
    ramfb.unwrap().set_notifier(Box::new(move |config, is_valid| {
        let h = Arc::clone(&hdl);
        rt.block_on(async move {
            let vnc = h.lock().await;
            vnc.server.update(config, is_valid).await;
        });
    }));

    // Save the newly created instance in the server's context.
    *context = Some(InstanceContext {
        instance: instance.clone(),
        properties,
        serial: com1,
        state_watcher: rx,
        serial_task: None,
    });
    drop(context);

    // Is this part of a migration?
    let migrate = if let Some(migrate_request) = request.migrate {
        // This is a migrate request and so we should try to establish a
        // connection with the source instance.
        let res = migrate::dest_initiate(rqctx, instance_id, migrate_request)
            .await
            .map_err(<_ as Into<HttpError>>::into)?;
        Some(res)
    } else {
        instance.on_transition(Box::new(move |next_state, _inv, ctx| {
            match next_state {
                propolis::instance::State::Boot => {
                    // Set vCPUs to their proper boot (INIT) state
                    for mut vcpu in ctx.mctx.vcpus() {
                        vcpu.reboot_state().unwrap();
                        vcpu.activate().unwrap();
                        // Set BSP to start up
                        if vcpu.is_bsp() {
                            vcpu.set_run_state(bhyve_api::VRS_RUN).unwrap();
                            vcpu.set_reg(
                                bhyve_api::vm_reg_name::VM_REG_GUEST_RIP,
                                0xfff0,
                            )
                            .unwrap();
                        }
                    }
                }
                _ => {}
            }
        }));

        None
    };

    instance.print();

    Ok(HttpResponseCreated(api::InstanceEnsureResponse { migrate }))
}

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}/uuid",
    unpublished = true,
}]
async fn instance_get_uuid(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstanceNameParams>,
) -> Result<HttpResponseOk<Uuid>, HttpError> {
    let context = rqctx.context().context.lock().await;

    let context = context.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

    if path_params.into_inner().instance_id != context.properties.name {
        return Err(HttpError::for_internal_error(
            "Instance name mismatch (path did not match struct)".to_string(),
        ));
    }

    Ok(HttpResponseOk(context.properties.id))
}

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<Context>>,
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
        disks: vec![],
        // TODO: Fix this; we need a way to enumerate attached NICs.
        // Possibly using the inventory of the instance?
        //
        // We *could* record whatever information about the NIC we want
        // when they're requested (adding fields to the server), but that
        // would make it difficult for Propolis to update any dynamic info
        // (i.e., has the device faulted, etc).
        nics: vec![],
    };

    Ok(HttpResponseOk(api::InstanceGetResponse { instance: instance_info }))
}

// TODO: Instance delete. What happens to the server? Does it shut down?

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}/state-monitor",
}]
async fn instance_state_monitor(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceStateMonitorRequest>,
) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError> {
    let (mut state_watcher, gen) = {
        let context = rqctx.context().context.lock().await;
        let context = context.as_ref().ok_or_else(|| {
            HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            )
        })?;
        let path_params = path_params.into_inner();
        if path_params.instance_id != context.properties.id {
            return Err(HttpError::for_internal_error(
                "UUID mismatch (path did not match struct)".to_string(),
            ));
        }

        let gen = request.into_inner().gen;
        let state_watcher = context.state_watcher.clone();

        (state_watcher, gen)
    };

    loop {
        let last = state_watcher.borrow().clone();
        if gen <= last.gen {
            let response = api::InstanceStateMonitorResponse {
                gen: last.gen,
                state: propolis_to_api_state(last.state),
            };
            return Ok(HttpResponseOk(response));
        }
        state_watcher.changed().await.unwrap();
    }
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/state",
}]
async fn instance_state_put(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceStateRequested>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
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
        HttpError::for_internal_error(format!("Failed to set state: {:?}", err))
    })?;

    Ok(HttpResponseUpdatedNoContent {})
}

async fn instance_serial_task(
    mut detach: oneshot::Receiver<()>,
    serial: Arc<Serial<LpcUart>>,
    ws_stream: WebSocketStream<Upgraded>,
    log: Logger,
    actx: &AsyncCtx,
) -> Result<(), SerialTaskError> {
    let mut output = [0u8; 1024];
    let mut cur_output: Option<Range<usize>> = None;
    let mut cur_input: Option<(Vec<u8>, usize)> = None;

    let (mut ws_sink, mut ws_stream) = ws_stream.split();
    loop {
        let (uart_read, ws_send) = match &cur_output {
            None => (
                serial.read_source(&mut output, actx).fuse(),
                Fuse::terminated(),
            ),
            Some(r) => (
                Fuse::terminated(),
                ws_sink.send(Message::binary(&output[r.clone()])).fuse(),
            ),
        };
        let (ws_recv, uart_write) = match &cur_input {
            None => (ws_stream.next().fuse(), Fuse::terminated()),
            Some((data, consumed)) => (
                Fuse::terminated(),
                serial.write_sink(&data[*consumed..], actx).fuse(),
            ),
        };

        tokio::select! {
            // Poll in the order written
            biased;

            // It's important we always poll the detach channel first
            // so that a constant stream of incoming/outgoing messages
            // don't cause us to ignore a detach
            _ = &mut detach => {
                info!(log, "Detaching from serial console");
                let close = CloseFrame {
                    code: CloseCode::Policy,
                    reason: Cow::Borrowed("serial console was detached"),
                };
                ws_sink.send(Message::Close(Some(close))).await?;
                break;
            }

            // Write bytes into the UART from the WS
            written = uart_write => {
                match written {
                    Some(0) | None => break,
                    Some(n) => {
                        let (data, consumed) = cur_input.as_mut().unwrap();
                        *consumed += n;
                        if *consumed == data.len() {
                            cur_input = None;
                        }
                    }
                }
            }

            // Transmit bytes from the UART through the WS
            write_success = ws_send => {
                write_success?;
                cur_output = None;
            }

            // Read bytes from the UART to be transmitted out the WS
            nread = uart_read => {
                match nread {
                    Some(0) | None => break,
                    Some(n) => { cur_output = Some(0..n) }
                }
            }

            // Receive bytes from the WS to be injected into the UART
            msg = ws_recv => {
                match msg {
                    Some(Ok(Message::Binary(input))) => {
                        cur_input = Some((input, 0));
                    }
                    Some(Ok(Message::Close(..))) | None => break,
                    _ => continue,
                }
            }
        }
    }
    Ok(())
}

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
) -> Result<Response<Body>, HttpError> {
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
    if context.serial_task.as_ref().map_or(false, |s| s.is_attached()) {
        return Err(HttpError::for_unavail(
            None,
            "serial console already attached".to_string(),
        ));
    }

    let serial = context
        .serial
        .as_ref()
        .ok_or_else(|| {
            HttpError::for_internal_error(
                "Instance present but serial not initialized".to_string(),
            )
        })?
        .clone();

    let request = &mut *rqctx.request.lock().await;

    if !request
        .headers()
        .get(header::CONNECTION)
        .and_then(|hv| hv.to_str().ok())
        .map(|hv| {
            hv.split(|c| c == ',' || c == ' ')
                .any(|vs| vs.eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false)
    {
        return Err(HttpError::for_bad_request(
            None,
            "expected connection upgrade".to_string(),
        ));
    }
    if !request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(|c| c == ',' || c == ' ')
                .any(|v| v.eq_ignore_ascii_case("websocket"))
        })
        .unwrap_or(false)
    {
        return Err(HttpError::for_bad_request(
            None,
            "unexpected protocol for upgrade".to_string(),
        ));
    }
    if request
        .headers()
        .get(header::SEC_WEBSOCKET_VERSION)
        .map(|v| v.as_bytes())
        != Some(b"13")
    {
        return Err(HttpError::for_bad_request(
            None,
            "missing or invalid websocket version".to_string(),
        ));
    }
    let accept_key = request
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|hv| hv.as_bytes())
        .map(|key| handshake::derive_accept_key(key))
        .ok_or_else(|| {
            HttpError::for_bad_request(
                None,
                "missing websocket key".to_string(),
            )
        })?;

    let (detach_ch, detach_recv) = oneshot::channel();

    let upgrade_fut = upgrade::on(request);
    let ws_log = rqctx.log.new(o!());
    let err_log = ws_log.clone();
    let actx = context.instance.async_ctx();
    let task = tokio::spawn(async move {
        let upgraded = match upgrade_fut.await {
            Ok(u) => u,
            Err(e) => {
                error!(err_log, "Serial Task Failed: {}", e);
                return;
            }
        };
        let config = WebSocketConfig {
            max_send_queue: Some(4096),
            ..Default::default()
        };
        let ws_stream = WebSocketStream::from_raw_socket(
            upgraded,
            Role::Server,
            Some(config),
        )
        .await;
        let _ =
            instance_serial_task(detach_recv, serial, ws_stream, ws_log, &actx)
                .await;
    });

    // Save active serial task handle
    context.serial_task = Some(SerialTask { task, detach_ch });

    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept_key)
        .body(Body::empty())?)
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/serial/detach",
}]
async fn instance_serial_detach(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
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

    let serial_task =
        context.serial_task.take().filter(|s| s.is_attached()).ok_or_else(
            || {
                HttpError::for_bad_request(
                    None,
                    "serial console already detached".to_string(),
                )
            },
        )?;

    serial_task.detach_ch.send(()).map_err(|_| {
        HttpError::for_internal_error(
            "couldn't send detach message to serial task".to_string(),
        )
    })?;
    let _ = serial_task.task.await.map_err(|_| {
        HttpError::for_internal_error(
            "failed to complete existing serial task".to_string(),
        )
    })?;

    Ok(HttpResponseUpdatedNoContent {})
}

// This endpoint is meant to only be called during a migration from the destination
// instance to the source instance as part of the HTTP connection upgrade used to
// establish the migration link. We don't actually want this exported via OpenAPI
// clients.
#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/migrate/start",
    unpublished = true,
}]
async fn instance_migrate_start(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceMigrateStartRequest>,
) -> Result<Response<Body>, HttpError> {
    let instance_id = path_params.into_inner().instance_id;
    let migration_id = request.into_inner().migration_id;
    migrate::source_start(rqctx, instance_id, migration_id)
        .await
        .map_err(Into::into)
}

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}/migrate/status"
}]
async fn instance_migrate_status(
    rqctx: Arc<RequestContext<Context>>,
    _path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceMigrateStatusRequest>,
) -> Result<HttpResponseOk<api::InstanceMigrateStatusResponse>, HttpError> {
    let migration_id = request.into_inner().migration_id;
    migrate::migrate_status(rqctx, migration_id)
        .await
        .map_err(Into::into)
        .map(HttpResponseOk)
}

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<Context> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get_uuid).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();
    api.register(instance_serial_detach).unwrap();
    api.register(instance_migrate_start).unwrap();
    api.register(instance_migrate_status).unwrap();
    api
}
