//! HTTP server callback functions.

use anyhow::Result;
use dropshot::{
    endpoint, ApiDescription, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, RequestContext, TypedBody,
};
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use hyper::upgrade::{self, Upgraded};
use hyper::{header, Body, Response, StatusCode};
use slog::{error, info, o, Logger};
use std::borrow::Cow;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch, Mutex};
use tokio_tungstenite::tungstenite::{
    self, handshake, protocol::Role, Message,
};
use tokio_tungstenite::WebSocketStream;
use thiserror::Error;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

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
    // Oneshot channel used to detach an attached serial session
    serial_detach: Option<oneshot::Sender<()>>,
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

    let properties = request.into_inner().properties;
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
        // TODO: The intial implementation does not modify any properties,
        // but we plausibly could do so - need to work out which properties
        // can be changed without rebooting.
        if ctx.properties != properties {
            return Err(HttpError::for_internal_error(
                "Cannot update running server".to_string(),
            ));
        }

        return Ok(HttpResponseCreated(api::InstanceEnsureResponse {}));
    }

    // Create the instance.
    //
    // The VM is named after the UUID, ensuring that it is unique.
    let lowmem = (properties.memory * 1024 * 1024) as usize;
    let instance =
        build_instance(&properties.id.to_string(), properties.vcpus, lowmem)
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
        serial_detach: None,
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
    detach: oneshot::Receiver<()>,
    serial: Arc<Mutex<Serial<DispCtx, LpcUart>>>,
    mut ws_stream: WebSocketStream<Upgraded>,
    log: Logger,
) -> Result<(), SerialTaskError> {
    let mut serial = serial.lock().await;
    let mut detach = detach.fuse();
    loop {
        let mut output = [0u8; 4096];
        futures::select! {
            result = serial.read(&mut output).fuse() => {
                if let Ok(n) = result {
                    ws_stream.send(Message::binary(&output[..n])).await?;
                }
            }
            msg = ws_stream.next().fuse() => {
                match msg {
                    Some(Ok(Message::Binary(input))) => {
                        serial.write_all(&input).await?;
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            _ = detach => {
                info!(log, "Detaching from serial console");
                let close = CloseFrame {
                    code: CloseCode::Policy,
                    reason: Cow::Borrowed("serial console was detached"),
                };
                ws_stream.send(Message::Close(Some(close))).await?;
                break;
            }
            complete => break
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
    if context.serial_detach.as_ref().map_or(false, |s| !s.is_closed()) {
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

    let (detach_sender, detach_recv) = oneshot::channel();
    context.serial_detach = Some(detach_sender);

    let upgrade_fut = upgrade::on(&mut *request);
    let serial = context.serial.clone();
    let ws_log = rqctx.log.new(o!());
    let err_log = ws_log.clone();
    tokio::spawn(
        async move {
            let upgraded = upgrade_fut.await?;
            let ws_stream =
                WebSocketStream::from_raw_socket(upgraded, Role::Server, None).await;
            instance_serial_task(detach_recv, serial, ws_stream, ws_log).await
        }
        .inspect_err(move |err| error!(err_log, "Serial Task Failed: {}", err)),
    );

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

    let serial_detach =
        context.serial_detach.take().filter(|s| !s.is_closed()).ok_or_else(
            || {
                HttpError::for_bad_request(
                    None,
                    "serial console already detached".to_string(),
                )
            },
        )?;

    serial_detach.send(()).map_err(|_| {
        HttpError::for_internal_error(
            "couldn't send detach message to serial task".to_string(),
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
