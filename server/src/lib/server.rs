//! HTTP server callback functions.

use anyhow::Result;
use dropshot::{
    endpoint, ApiDescription, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, RequestContext, TypedBody,
};
use futures::future::FusedFuture;
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use hyper::upgrade::{self, Upgraded};
use hyper::{header, Body, Response, StatusCode};
use slog::{error, info, o, Logger};
use std::borrow::Cow;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{
    self, handshake, protocol::Role, Message,
};
use tokio_tungstenite::WebSocketStream;

use propolis::dispatch::DispCtx;
use propolis::hw::chipset::Chipset;
use propolis::hw::pci;
use propolis::hw::uart::LpcUart;
use propolis::instance::Instance;
use propolis_client::api;

use crate::config::Config;
use crate::initializer::{build_instance, MachineInitializer};
use crate::serial::Serial;

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
    task: JoinHandle<Result<(), SerialTaskError>>,
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
struct InstanceContext {
    // The instance, which may or may not be instantiated.
    instance: Arc<Instance>,
    properties: api::InstanceProperties,
    serial: Arc<Mutex<Serial<DispCtx, LpcUart>>>,
    state_watcher: watch::Receiver<StateChange>,
    serial_task: Option<SerialTask>,
}

/// Contextual information accessible from HTTP callbacks.
pub struct Context {
    context: Mutex<Option<InstanceContext>>,
    config: Config,
}

impl Context {
    /// Creates a new server context object.
    pub fn new(config: Config) -> Self {
        Context { context: Mutex::new(None), config }
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
        PropolisState::Quiesce => ApiState::Stopping,
        PropolisState::Halt => ApiState::Stopped,
        PropolisState::Reset => ApiState::Rebooting,
        PropolisState::Destroy => ApiState::Destroyed,
    }
}

#[derive(Clone, Copy, Debug)]
enum SlotType {
    NIC,
    #[allow(dead_code)]
    Disk,
}

// This is a somewhat hard-coded translation of a stable "PCI slot" to a BDF.
//
// For all the devices requested by Nexus (network interfaces, disks, etc),
// we'd like to assign a stable PCI slot, such that re-allocating these
// devices on a new instance of propolis produces the same guest-visible
// BDFs.
fn slot_to_bdf(slot: api::Slot, ty: SlotType) -> Result<pci::Bdf> {
    match ty {
        // Slots for NICS: 0x08 -> 0x0F
        SlotType::NIC if slot.0 <= 7 => Ok(pci::Bdf::new(0, slot.0 + 0x8, 0)),
        // Slots for Disks: 0x10 -> 0x17
        SlotType::Disk if slot.0 <= 7 => Ok(pci::Bdf::new(0, slot.0 + 0x10, 0)),
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
    let (properties, nics) = (request.properties, request.nics);
    if path_params.into_inner().instance_id != properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }

    // Handle requsts to an instance that has already been initialized.
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

        return Ok(HttpResponseCreated(api::InstanceEnsureResponse {}));
    }

    const MB: usize = 1024 * 1024;
    const GB: usize = 1024 * 1024 * 1024;
    let memsize = properties.memory as usize * MB;
    let lowmem = memsize.min(3 * GB);
    let highmem = memsize.saturating_sub(3 * GB);

    // Create the instance.
    //
    // The VM is named after the UUID, ensuring that it is unique.
    let instance = build_instance(
        &properties.id.to_string(),
        properties.vcpus,
        lowmem,
        highmem,
    )
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
    let mut com1: Option<Serial<DispCtx, LpcUart>> = None;

    instance
        .initialize(|machine, mctx, disp, inv| {
            let init = MachineInitializer::new(machine, mctx, disp, inv);
            init.initialize_rom(server_context.config.get_bootrom())?;
            machine.initialize_rtc(lowmem, highmem).unwrap();
            let chipset = init.initialize_chipset()?;
            com1 = Some(init.initialize_uart(&chipset)?);
            init.initialize_ps2(&chipset)?;
            init.initialize_qemu_debug_port(&chipset)?;

            // Attach devices which have been requested from the HTTP interface.
            for nic in &nics {
                let bdf =
                    slot_to_bdf(nic.slot, SlotType::NIC).map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidData,
                            format!("Cannot parse vnic PCI: {}", e),
                        )
                    })?;
                init.initialize_vnic(&chipset, &nic.name, bdf)?;
            }

            // Attach devices which are hard-coded in the config.
            //
            // NOTE: This interface is effectively a stop-gap for development
            // purposes. Longer term, peripherals will be attached via separate
            // HTTP interfaces.
            for (_, dev) in server_context.config.devs() {
                let driver = &dev.driver as &str;
                match driver {
                    "pci-virtio-block" => {
                        let block_dev_name = dev
                            .options
                            .get("block_dev")
                            .unwrap()
                            .as_str()
                            .unwrap();

                        let block_dev = server_context
                            .config
                            .block_dev::<propolis::hw::virtio::block::Request>(
                            block_dev_name,
                        );

                        let bdf: pci::Bdf =
                            dev.get("pci-path").ok_or_else(|| {
                                Error::new(
                                    ErrorKind::InvalidData,
                                    "Cannot parse disk PCI",
                                )
                            })?;

                        init.initialize_block(
                            &chipset,
                            bdf,
                            block_dev_name,
                            block_dev,
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

            // Finalize device.
            chipset.device().pci_finalize(&disp.ctx());
            init.initialize_fwcfg(&chipset, properties.vcpus)?;
            init.initialize_cpus()?;
            Ok(())
        })
        .map_err(|err| {
            HttpError::for_internal_error(format!(
                "Failed to initialize machine: {}",
                err.to_string()
            ))
        })?;

    let (tx, rx) = watch::channel(StateChange {
        gen: 0,
        state: propolis::instance::State::Initialize,
    });
    instance.print();

    instance.on_transition(Box::new(move |next_state| {
        println!("state cb: {:?}", next_state);
        let last = (*tx.borrow()).clone();
        let _ = tx.send(StateChange { gen: last.gen + 1, state: next_state });
    }));

    // Save the newly created instance in the server's context.
    *context = Some(InstanceContext {
        instance,
        properties,
        serial: Arc::new(Mutex::new(com1.unwrap())),
        state_watcher: rx,
        serial_task: None,
    });

    Ok(HttpResponseCreated(api::InstanceEnsureResponse {}))
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
        HttpError::for_internal_error(format!("Failed to set state: {}", err))
    })?;

    Ok(HttpResponseUpdatedNoContent {})
}

async fn instance_serial_task(
    mut detach: oneshot::Receiver<()>,
    serial: Arc<Mutex<Serial<DispCtx, LpcUart>>>,
    mut ws_stream: WebSocketStream<Upgraded>,
    log: Logger,
) -> Result<(), SerialTaskError> {
    let mut serial = serial.lock().await;
    let read_ready_fut = futures::future::Fuse::terminated();
    tokio::pin!(read_ready_fut);

    loop {
        let mut output = [0u8; 4096];
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
                ws_stream.send(Message::Close(Some(close))).await?;
                break;
            }

            // Go ahead and try to read from the serial port.
            // Backpressure: The `read_ready_fut` precondition makes sure we only try to read
            // from the serial port until either the buffer has filled completely, or
            // (for partial reads) 10ms have elapsed. This prevents spinning on a
            // serial console which is emitting single bytes at a time.
            Ok(n) = serial.read(&mut output), if read_ready_fut.is_terminated() => {
                ws_stream.send(Message::binary(&output[..n])).await?;

                // Queue another read/timeout
                read_ready_fut.set(timeout(Duration::from_millis(10), serial.read_buffer_full()).fuse());
            }

            // Then check for any incoming data from the client
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Binary(input))) => serial.write_all(&input).await?,
                    Some(Ok(Message::Close(..))) | None => break,
                    _ => continue,
                }
            }

            // Poll timeout/read_buffer_full future; see above comment
            _ = &mut read_ready_fut => {}
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

    let upgrade_fut = upgrade::on(&mut *request);
    let serial = context.serial.clone();
    let ws_log = rqctx.log.new(o!());
    let err_log = ws_log.clone();
    let task = tokio::spawn(
        async move {
            let upgraded = upgrade_fut.await?;
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
            instance_serial_task(detach_recv, serial, ws_stream, ws_log).await
        }
        .inspect_err(move |err| error!(err_log, "Serial Task Failed: {}", err)),
    );

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

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<Context> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();
    api.register(instance_serial_detach).unwrap();
    api
}
