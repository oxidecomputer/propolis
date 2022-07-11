//! HTTP server callback functions.

use anyhow::Result;
use dropshot::{
    endpoint, ApiDescription, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, RequestContext, TypedBody,
};
use futures::future::Fuse;
use futures::stream::{SplitSink, SplitStream};
use futures::{FutureExt, SinkExt, StreamExt};
use hyper::upgrade::{self, Upgraded};
use hyper::{header, Body, Response, StatusCode};
use propolis::hw::qemu::ramfb::RamFb;
use rfb::server::VncServer;
use slog::{error, info, o, Logger};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, handshake, Message};
use tokio_tungstenite::WebSocketStream;

use propolis::dispatch::AsyncCtx;
use propolis::hw::uart::LpcUart;
use propolis::instance::Instance;
use propolis_client::{api, instance_spec};

use crate::config::Config;
use crate::initializer::{build_instance, MachineInitializer};
use crate::serial::Serial;
use crate::spec::SpecBuilder;
use crate::stats::{prop_oximeter, PropCountStat, PropStatOuter};
use crate::vnc::PropolisVncServer;
use crate::{migrate, vnc};
use uuid::Uuid;

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
    /// Oneshot channel used to signal the task to terminate gracefully
    close_ch: Option<oneshot::Sender<()>>,
    /// Channel used to send new client connections to the streaming task
    websocks_ch: mpsc::Sender<WebSocketStream<Upgraded>>,
}

impl Drop for SerialTask {
    fn drop(&mut self) {
        if let Some(ch) = self.close_ch.take() {
            let _ = ch.send(());
        } else {
            self.task.abort();
        }
    }
}

#[derive(Clone)]
struct StateChange {
    gen: u64,
    state: propolis::instance::State,
}

pub(crate) type CrucibleBackendMap =
    BTreeMap<Uuid, Arc<propolis::block::CrucibleBackend>>;

// All context for a single propolis instance.
pub(crate) struct InstanceContext {
    // The instance, which may or may not be instantiated.
    pub instance: Arc<Instance>,
    pub properties: api::InstanceProperties,
    serial: Option<Arc<Serial<LpcUart>>>,
    state_watcher: watch::Receiver<StateChange>,
    serial_task: Option<SerialTask>,

    /// A map of disk names to CrucibleBackend
    pub(crate) crucible_backends: Mutex<CrucibleBackendMap>,
}

#[derive(Debug, Clone)]
pub struct InstanceMetricsConfig {
    pub propolis_addr: SocketAddr,
    pub metric_addr: SocketAddr,
}
impl InstanceMetricsConfig {
    pub fn new(propolis_addr: SocketAddr, metric_addr: SocketAddr) -> Self {
        InstanceMetricsConfig { propolis_addr, metric_addr }
    }
}

/// Contextual information accessible from HTTP callbacks.
pub struct Context {
    pub(crate) context: Mutex<Option<InstanceContext>>,
    pub(crate) migrate_task: Mutex<Option<migrate::MigrateTask>>,
    config: Config,
    log: Logger,
    pub(crate) vnc_server: Arc<Mutex<VncServer<PropolisVncServer>>>,
    pub(crate) use_reservoir: bool,
    // To register with Oximeter.
    pub(crate) metric_config: Option<InstanceMetricsConfig>,
    pub instance_metrics: Mutex<Option<PropStatOuter>>,
}

impl Context {
    /// Creates a new server context object.
    pub fn new(
        config: Config,
        vnc_server: VncServer<PropolisVncServer>,
        use_reservoir: bool,
        log: Logger,
        metric_config: Option<InstanceMetricsConfig>,
    ) -> Self {
        Context {
            context: Mutex::new(None),
            migrate_task: Mutex::new(None),
            config,
            log,
            vnc_server: Arc::new(Mutex::new(vnc_server)),
            use_reservoir,
            metric_config,
            instance_metrics: Mutex::new(None),
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
        ApiState::MigrateStart => PropolisState::MigrateStart,
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

/*
 * Instances: CRUD API
 */

#[endpoint {
    method = PUT,
    path = "/instance",
}]
async fn instance_ensure(
    rqctx: Arc<RequestContext<Context>>,
    request: TypedBody<api::InstanceEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    let server_context = rqctx.context();

    let request = request.into_inner();
    let (properties, nics, disks, cloud_init_bytes) = (
        request.properties,
        request.nics,
        request.disks,
        request.cloud_init_bytes,
    );

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

    // If anyone outside Propolis wishes to register for metrics, this
    // will hold the producer registry they can use.
    let mut producer_registry = None;

    // Determine if we need to setup the metrics endpoint or not.
    // If we do, we will then populate producer_registry with something.
    if server_context.metric_config.is_some() {
        // Create some propolis level metrics.
        let prop_count_stat = PropCountStat::new(properties.id.clone());
        let pso = PropStatOuter {
            prop_stat_wrap: Arc::new(std::sync::Mutex::new(prop_count_stat)),
        };

        // This is the address where stats will be collected.
        let propolis_addr =
            server_context.metric_config.as_ref().unwrap().propolis_addr.ip();
        let listen_addr = SocketAddr::new(propolis_addr, 0);
        let register_addr =
            server_context.metric_config.as_ref().unwrap().metric_addr;

        match prop_oximeter(
            properties.id.clone(),
            listen_addr,
            register_addr,
            rqctx.log.clone(),
        )
        .await
        {
            Err(e) => {
                error!(rqctx.log, "Failed to register with Oximeter {:?}", e);
            }
            Ok(server) => {
                info!(
                    rqctx.log,
                    "registering metrics with instance uuid: {}", properties.id,
                );
                // Register the propolis level instance metrics.
                server.registry().register_producer(pso.clone()).unwrap();

                // Now that our metrics are registered, attach them to
                // the server context so they can be updated.
                let mut im = server_context.instance_metrics.lock().await;
                *im = Some(pso.clone());
                drop(im);

                // Clone the producer_registry that we can pass to any
                // other library that may want to register their own
                // metrics.  Doing it this way means propolis does not have
                // to know what metrics they register.
                producer_registry = Some(server.registry().clone());

                // Spawn the metric endpoint.
                tokio::spawn(async move {
                    server.serve_forever().await.unwrap();
                });
            }
        }
    } else {
        info!(rqctx.log, "No metrics registration was requested");
    }

    let mut in_memory_disk_contents: BTreeMap<String, Vec<u8>> =
        BTreeMap::new();
    let mut spec_builder =
        SpecBuilder::new(&properties, &server_context.config).map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to build instance spec: {}", e),
            )
        })?;
    for nic in &nics {
        spec_builder.add_nic_from_request(nic).map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to add requested NIC: {}", e),
            )
        })?;
    }
    for disk in &disks {
        spec_builder.add_disk_from_request(disk).map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to add requested disk: {}", e),
            )
        })?;
    }
    if let Some(as_base64) = cloud_init_bytes {
        let bytes = base64::decode(&as_base64).map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to decode cloud-init bytes: {}", e),
            )
        })?;
        spec_builder.add_cloud_init_from_request().map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to add requested cloud-init bytes: {}", e),
            )
        })?;
        in_memory_disk_contents.insert("cloud-init".to_string(), bytes);
    }
    spec_builder.add_devices_from_config(&server_context.config).map_err(
        |e| {
            HttpError::for_internal_error(format!(
                "failed to add static devices from config: {}",
                e
            ))
        },
    )?;
    for port in [
        instance_spec::SerialPortNumber::Com1,
        instance_spec::SerialPortNumber::Com2,
        instance_spec::SerialPortNumber::Com3,
        instance_spec::SerialPortNumber::Com4,
    ] {
        spec_builder.add_serial_port(port).map_err(|e| {
            HttpError::for_internal_error(format!(
                "failed to add serial port {:?} to spec: {}",
                port, e
            ))
        })?;
    }
    let spec = spec_builder.finish();

    // Create child logger for instance-related messages
    let vmm_log = server_context.log.new(o!("component" => "vmm"));

    // Create the instance.
    //
    // The VM is named after the UUID, ensuring that it is unique.
    let instance = build_instance(
        &properties.id.to_string(),
        &spec,
        server_context.use_reservoir,
        vmm_log,
    )
    .map_err(|err| {
        HttpError::for_internal_error(format!("Cannot build instance: {}", err))
    })?;

    let (tx, rx) = watch::channel(StateChange {
        gen: 0,
        state: propolis::instance::State::Initialize,
    });

    instance.on_transition(Box::new(move |next_state, _, _inv, _ctx| {
        let last = (*tx.borrow()).clone();
        let _ = tx.send(StateChange { gen: last.gen + 1, state: next_state });
    }));

    let mut com1 = None;
    let mut ramfb: Option<Arc<RamFb>> = None;
    let mut rt_handle = None;
    let mut crucible_backends = BTreeMap::new();

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
                &spec,
                producer_registry.clone(),
            );
            init.initialize_rom(server_context.config.get_bootrom())?;
            init.initialize_kernel_devs()?;

            let chipset = init.initialize_chipset()?;
            com1 = Some(Arc::new(init.initialize_uart(&chipset)?));
            init.initialize_ps2(&chipset)?;
            init.initialize_qemu_debug_port()?;
            init.initialize_network_devices(&chipset)?;
            crucible_backends = init.initialize_storage_devices(
                &chipset,
                in_memory_disk_contents,
            )?;
            info!(
                server_context.log,
                "Initialized {} Crucible backends: {:?}",
                crucible_backends.len(),
                crucible_backends.keys()
            );
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
    let fb_spec = ramfb.as_ref().unwrap().get_framebuffer_spec();
    let fb = vnc::RamFb::new(fb_spec.clone());
    let actx = instance.async_ctx();
    let vnc_server = vnc_hdl.lock().await;
    vnc_server.server.initialize(fb, actx, vnc_server.clone()).await;

    let rt = rt_handle.unwrap();
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
        crucible_backends: Mutex::new(crucible_backends),
    });
    drop(context);

    // Is this part of a migration?
    let migrate = if let Some(migrate_request) = request.migrate {
        // This is a migrate request and so we should try to establish a
        // connection with the source instance.
        let res = migrate::dest_initiate(rqctx, migrate_request)
            .await
            .map_err(<_ as Into<HttpError>>::into)?;
        Some(res)
    } else {
        None
    };

    instance.print();

    Ok(HttpResponseCreated(api::InstanceEnsureResponse { migrate }))
}

#[endpoint {
    method = GET,
    path = "/instance",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<Context>>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    let context = rqctx.context().context.lock().await;

    let context = context.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

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
    path = "/instance/state-monitor",
}]
async fn instance_state_monitor(
    rqctx: Arc<RequestContext<Context>>,
    request: TypedBody<api::InstanceStateMonitorRequest>,
) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError> {
    let (mut state_watcher, gen) = {
        let context = rqctx.context().context.lock().await;
        let context = context.as_ref().ok_or_else(|| {
            HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            )
        })?;

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
    path = "/instance/state",
}]
async fn instance_state_put(
    rqctx: Arc<RequestContext<Context>>,
    request: TypedBody<api::InstanceStateRequested>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let context = rqctx.context().context.lock().await;

    let context = context.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

    let state = api_to_propolis_state(request.into_inner());
    context.instance.set_target_state(state).map_err(|err| {
        HttpError::for_internal_error(format!("Failed to set state: {:?}", err))
    })?;

    // Update the metrics counter when we apply a reset
    if state == propolis::instance::ReqState::Reset {
        let server_context = rqctx.context();
        let instance_metrics = server_context.instance_metrics.lock().await;
        if let Some(im) = &*instance_metrics {
            im.count_reset();
        }
    }

    Ok(HttpResponseUpdatedNoContent {})
}

async fn instance_serial_task(
    mut websocks_recv: mpsc::Receiver<WebSocketStream<Upgraded>>,
    mut close_recv: oneshot::Receiver<()>,
    serial: Arc<Serial<LpcUart>>,
    log: Logger,
    actx: &AsyncCtx,
) -> Result<(), SerialTaskError> {
    let mut output = [0u8; 1024];
    let mut cur_output: Option<Range<usize>> = None;
    let mut cur_input: Option<(Vec<u8>, usize)> = None;

    let mut ws_sinks: Vec<SplitSink<WebSocketStream<Upgraded>, Message>> =
        Vec::new();
    let mut ws_streams: Vec<SplitStream<WebSocketStream<Upgraded>>> =
        Vec::new();

    let (send_ch, mut recv_ch) = mpsc::channel(4);

    loop {
        let (uart_read, ws_send) =
            match &cur_output {
                None => (
                    serial.read_source(&mut output, actx).fuse(),
                    Fuse::terminated(),
                ),
                Some(r) => (
                    Fuse::terminated(),
                    if !ws_sinks.is_empty() {
                        futures::stream::iter(ws_sinks.iter_mut().zip(
                            std::iter::repeat(Vec::from(&output[r.clone()])),
                        ))
                        .for_each_concurrent(4, |(ws, bin)| {
                            ws.send(Message::binary(bin)).map(|_| ())
                        })
                        .fuse()
                    } else {
                        Fuse::terminated()
                    },
                ),
            };

        let (ws_recv, uart_write) = match &cur_input {
            None => (
                if !ws_streams.is_empty() {
                    futures::stream::iter(ws_streams.iter_mut().enumerate())
                        .for_each_concurrent(4, |(i, ws)| {
                            // if we don't `move` below, rustc says that `i`
                            // (which is usize: Copy (!)) is borrowed. but if we
                            // move without making this explicit reference here,
                            // it moves send_ch into the closure.
                            let ch = &send_ch;
                            ws.next()
                                .then(move |msg| ch.send((i, msg)))
                                .map(|_| ())
                        })
                        .fuse()
                } else {
                    Fuse::terminated()
                },
                Fuse::terminated(),
            ),
            Some((data, consumed)) => (
                Fuse::terminated(),
                serial.write_sink(&data[*consumed..], actx).fuse(),
            ),
        };

        let recv_ch_fut = recv_ch.recv().fuse();

        tokio::select! {
            // Poll in the order written
            biased;

            // It's important we always poll the close channel first
            // so that a constant stream of incoming/outgoing messages
            // don't cause us to ignore it
            _ = &mut close_recv => {
                info!(log, "Terminating serial task");
                break;
            }

            new_ws = websocks_recv.recv() => {
                if let Some(ws) = new_ws {
                    let (ws_sink, ws_stream) = ws.split();
                    ws_sinks.push(ws_sink);
                    ws_streams.push(ws_stream);
                }
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
            _ = ws_send => {
                cur_output = None;
            }

            // Read bytes from the UART to be transmitted out the WS
            nread = uart_read => {
                match nread {
                    Some(0) | None => break,
                    Some(n) => { cur_output = Some(0..n) }
                }
            }

            // Receive bytes from the intermediate channel to be injected into
            // the UART. This needs to be checked before `ws_recv` so that
            // "close" messages can be processed and their indicated
            // sinks/streams removed before they are polled again.
            pair = recv_ch_fut => {
                if let Some((i, msg)) = pair {
                    match msg {
                        Some(Ok(Message::Binary(input))) => {
                            cur_input = Some((input, 0));
                        }
                        Some(Ok(Message::Close(..))) | None => {
                            info!(log, "Removed a closed serial connection.");
                            let _ = ws_sinks.remove(i).close().await;
                            let _ = ws_streams.remove(i);
                        },
                        _ => continue,
                    }
                }
            }

            // Receive bytes from connected WS clients to feed to the
            // intermediate recv_ch
            _ = ws_recv => {}
        }
    }
    Ok(())
}

#[endpoint {
    method = GET,
    path = "/instance/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<Context>>,
) -> Result<Response<Body>, HttpError> {
    let mut context = rqctx.context().context.lock().await;

    let context = context.as_mut().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

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

    let actx = context.instance.async_ctx();
    let ws_log = rqctx.log.new(o!());
    let err_log = ws_log.clone();

    // Create or get active serial task handle and channels
    let serial_task = context.serial_task.get_or_insert_with(move || {
        let (websocks_ch, websocks_recv) = mpsc::channel(1);
        let (close_ch, close_recv) = oneshot::channel();

        let task = tokio::spawn(async move {
            if let Err(e) = instance_serial_task(
                websocks_recv,
                close_recv,
                serial,
                ws_log.clone(),
                &actx,
            )
            .await
            {
                error!(ws_log, "Failed to spawn instance serial task: {}", e);
            }
        });

        SerialTask { task, close_ch: Some(close_ch), websocks_ch }
    });

    let upgrade_fut = upgrade::on(request);
    let config =
        WebSocketConfig { max_send_queue: Some(4096), ..Default::default() };
    let websocks_send = serial_task.websocks_ch.clone();
    tokio::spawn(async move {
        let upgraded = match upgrade_fut.await {
            Ok(u) => u,
            Err(e) => {
                error!(err_log, "Serial socket upgrade failed: {}", e);
                return;
            }
        };

        let ws_stream = WebSocketStream::from_raw_socket(
            upgraded,
            Role::Server,
            Some(config),
        )
        .await;

        if let Err(e) = websocks_send.send(ws_stream).await {
            error!(err_log, "Serial socket hand-off failed: {}", e);
        }
    });

    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept_key)
        .body(Body::empty())?)
}

// This endpoint is meant to only be called during a migration from the destination
// instance to the source instance as part of the HTTP connection upgrade used to
// establish the migration link. We don't actually want this exported via OpenAPI
// clients.
#[endpoint {
    method = PUT,
    path = "/instance/migrate/start",
    unpublished = true,
}]
async fn instance_migrate_start(
    rqctx: Arc<RequestContext<Context>>,
    request: TypedBody<api::InstanceMigrateStartRequest>,
) -> Result<Response<Body>, HttpError> {
    let migration_id = request.into_inner().migration_id;
    migrate::source_start(rqctx, migration_id).await.map_err(Into::into)
}

#[endpoint {
    method = GET,
    path = "/instance/migrate/status"
}]
async fn instance_migrate_status(
    rqctx: Arc<RequestContext<Context>>,
    request: TypedBody<api::InstanceMigrateStatusRequest>,
) -> Result<HttpResponseOk<api::InstanceMigrateStatusResponse>, HttpError> {
    let migration_id = request.into_inner().migration_id;
    migrate::migrate_status(rqctx, migration_id)
        .await
        .map_err(Into::into)
        .map(HttpResponseOk)
}

/// Issue a snapshot request to a crucible backend
#[endpoint {
    method = POST,
    path = "/instance/disk/{id}/snapshot/{snapshot_id}",
}]
async fn instance_issue_crucible_snapshot_request(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::SnapshotRequestPathParams>,
) -> Result<HttpResponseOk<()>, HttpError> {
    let context = rqctx.context().context.lock().await;
    let path_params = path_params.into_inner();

    let context = context.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;

    let crucible_backends = context.crucible_backends.lock().await;
    let crucible_backend = crucible_backends.get(&path_params.id);

    if let Some(crucible_backend) = crucible_backend {
        crucible_backend.snapshot(path_params.snapshot_id).map_err(|e| {
            HttpError::for_bad_request(Some(e.to_string()), e.to_string())
        })?;

        Ok(HttpResponseOk(()))
    } else {
        let s = format!("no disk with id {}!", path_params.id);
        Err(HttpError::for_not_found(Some(s.clone()), s))
    }
}

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<Context> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();
    api.register(instance_migrate_start).unwrap();
    api.register(instance_migrate_status).unwrap();
    api.register(instance_issue_crucible_snapshot_request).unwrap();

    api
}
