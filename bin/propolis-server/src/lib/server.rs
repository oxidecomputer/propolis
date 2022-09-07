//! HTTP server callback functions.
//!
//! Functions in this module verify parameters and convert between types (API
//! request types to Propolis-native types and Propolis-native error types to
//! HTTP error codes) before sending operations to other components (e.g. the VM
//! controller) for processing.

use std::sync::Arc;
use std::{collections::BTreeMap, net::SocketAddr};

use dropshot::{
    endpoint, ApiDescription, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, RequestContext, TypedBody,
};
use hyper::StatusCode;
use hyper::{http::header, upgrade, Body, Response};
use oximeter::types::ProducerRegistry;
use propolis_client::instance_spec;
use propolis_client::{api, instance_spec::InstanceSpec};
use propolis_server_config::Config as VmTomlConfig;
use rfb::server::VncServer;
use slog::{error, o, Logger};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, MappedMutexGuard, Mutex, MutexGuard};
use tokio_tungstenite::tungstenite::handshake;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use crate::spec::{SpecBuilder, SpecBuilderError};
use crate::vm::VmController;
use crate::vnc::PropolisVncServer;

pub(crate) type CrucibleBackendMap =
    BTreeMap<uuid::Uuid, Arc<propolis::block::CrucibleBackend>>;

/// Configuration used to set this server up to provide Oximeter metrics.
#[derive(Debug, Clone)]
pub struct MetricsEndpointConfig {
    /// The address at which the Oximeter endpoint will be hosted (i.e., this
    /// server's address).
    pub propolis_addr: SocketAddr,

    /// The address of the Oximeter instance that should be told that this
    /// endpoint is available.
    pub metric_addr: SocketAddr,
}

impl MetricsEndpointConfig {
    pub fn new(propolis_addr: SocketAddr, metric_addr: SocketAddr) -> Self {
        Self { propolis_addr, metric_addr }
    }
}

/// Static configuration for objects owned by this server. The server obtains
/// this configuration at startup time and refers to it when manipulating its
/// objects.
pub struct StaticConfig {
    /// The TOML-driven configuration for this server's instances.
    pub vm: VmTomlConfig,

    /// Whether to use the host's guest memory reservoir to back guest memory.
    pub use_reservoir: bool,

    /// The configuration to use when setting up this server's Oximeter
    /// endpoint.
    metrics: Option<MetricsEndpointConfig>,
}

/// The state of the current VM controller in this server, if there is one, or
/// the most recently created one, if one ever existed.
pub enum VmControllerState {
    /// No VM controller has ever been constructed in this server.
    NotCreated,

    /// A VM controller exists.
    Created(Arc<VmController>),

    /// No VM controller exists. The attached value describes that instance's
    /// properties.
    Destroyed(propolis_client::api::Instance),
}

impl VmControllerState {
    pub fn controller_mut(&mut self) -> Option<&mut Arc<VmController>> {
        match self {
            VmControllerState::NotCreated => None,
            VmControllerState::Created(c) => Some(c),
            VmControllerState::Destroyed(_) => None,
        }
    }

    pub fn take_vm(&mut self) -> Option<Arc<VmController>> {
        if let VmControllerState::Created(vm) = self {
            let inst = propolis_client::api::Instance {
                properties: vm.properties().clone(),
                state: propolis_client::api::InstanceState::Destroyed,
                disks: vec![],
                nics: vec![],
            };

            if let VmControllerState::Created(vm) =
                std::mem::replace(self, VmControllerState::Destroyed(inst))
            {
                Some(vm)
            } else {
                unreachable!()
            }
        } else {
            None
        }
    }
}

/// Objects that this server creates, owns, and manipulates in response to API
/// calls.
pub struct ServiceProviders {
    /// The VM controller that manages this server's Propolis instance. This is
    /// `None` until a guest is created via `instance_ensure`.
    pub vm: Mutex<VmControllerState>,

    serial_task: Mutex<Option<super::serial::SerialTask>>,

    oximeter_server_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    oximeter_stats: Mutex<Option<crate::stats::ServerStatsOuter>>,

    vnc_server: Arc<Mutex<VncServer<PropolisVncServer>>>,
}

impl ServiceProviders {
    /// Directs the current set of per-instance service providers to stop in an
    /// orderly fashion, then drops them all.
    async fn stop(&self, log: &Logger) {
        if let Some(vm) = self.vm.lock().await.take_vm() {
            slog::info!(log, "Dropping instance";
                        "strong_refs" => Arc::strong_count(&vm),
                        "weak_refs" => Arc::weak_count(&vm));
        }
        if let Some(mut serial_task) = self.serial_task.lock().await.take() {
            if let Some(close) = serial_task.close_ch.take() {
                let _ = close.send(());
            }
        }
        if let Some(server) = self.oximeter_server_task.lock().await.take() {
            server.abort();
        }
        let _ = self.oximeter_stats.lock().await.take();
    }
}

/// Context accessible from HTTP callbacks.
pub struct DropshotEndpointContext {
    static_config: StaticConfig,
    pub services: ServiceProviders,
    log: Logger,
}

impl DropshotEndpointContext {
    /// Creates a new server context object.
    pub fn new(
        config: VmTomlConfig,
        vnc_server: VncServer<PropolisVncServer>,
        use_reservoir: bool,
        log: slog::Logger,
        metric_config: Option<MetricsEndpointConfig>,
    ) -> Self {
        Self {
            static_config: StaticConfig {
                vm: config,
                use_reservoir,
                metrics: metric_config,
            },
            services: ServiceProviders {
                vm: Mutex::new(VmControllerState::NotCreated),
                serial_task: Mutex::new(None),
                oximeter_server_task: Mutex::new(None),
                oximeter_stats: Mutex::new(None),
                vnc_server: Arc::new(Mutex::new(vnc_server)),
            },
            log,
        }
    }

    /// Get access to the VM controller for this context, emitting a consistent
    /// error if it is absent.
    pub(crate) async fn vm(
        &self,
    ) -> Result<MappedMutexGuard<Arc<VmController>>, HttpError> {
        MutexGuard::try_map(
            self.services.vm.lock().await,
            VmControllerState::controller_mut,
        )
        .map_err(|_| {
            HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            )
        })
    }
}

#[derive(Debug, Error)]
enum SpecCreationError {
    #[error(transparent)]
    SpecBuilderError(#[from] SpecBuilderError),

    #[error(transparent)]
    Base64DecodeError(#[from] base64::DecodeError),
}

/// Creates an instance spec from an ensure request. (Both types are foreign to
/// this crate, so implementing TryFrom for them is not allowed.)
fn instance_spec_from_request(
    request: &propolis_client::api::InstanceEnsureRequest,
    toml_config: &VmTomlConfig,
) -> Result<(InstanceSpec, BTreeMap<String, Vec<u8>>), SpecCreationError> {
    let mut in_memory_disk_contents: BTreeMap<String, Vec<u8>> =
        BTreeMap::new();
    let mut spec_builder = SpecBuilder::new(&request.properties, toml_config)?;
    for nic in &request.nics {
        spec_builder.add_nic_from_request(nic)?;
    }

    for disk in &request.disks {
        spec_builder.add_disk_from_request(disk)?;
    }

    if let Some(as_base64) = &request.cloud_init_bytes {
        let bytes = base64::decode(as_base64)?;
        spec_builder.add_cloud_init_from_request()?;
        in_memory_disk_contents.insert("cloud-init".to_string(), bytes);
    }

    spec_builder.add_devices_from_config(toml_config)?;
    for port in [
        instance_spec::SerialPortNumber::Com1,
        instance_spec::SerialPortNumber::Com2,
        instance_spec::SerialPortNumber::Com3,
        instance_spec::SerialPortNumber::Com4,
    ] {
        spec_builder.add_serial_port(port)?;
    }

    Ok((spec_builder.finish(), in_memory_disk_contents))
}

/// Attempts to register an Oximeter server reporting metrics from a new
/// instance, returning the producer registry from that server on success.
async fn register_oximeter(
    server_context: &DropshotEndpointContext,
    cfg: &MetricsEndpointConfig,
    vm_id: uuid::Uuid,
    log: Logger,
) -> anyhow::Result<ProducerRegistry> {
    assert!(server_context.services.oximeter_stats.lock().await.is_none());
    assert!(server_context
        .services
        .oximeter_server_task
        .lock()
        .await
        .is_none());

    let server = crate::stats::start_oximeter_server(vm_id, cfg, log).await?;
    let stats = crate::stats::register_server_metrics(vm_id, &server)?;
    *server_context.services.oximeter_stats.lock().await = Some(stats);
    let registry = server.registry().clone();
    let server_task = tokio::spawn(async move {
        server.serve_forever().await.unwrap();
    });

    *server_context.services.oximeter_server_task.lock().await =
        Some(server_task);

    Ok(registry)
}

#[endpoint {
    method = PUT,
    path = "/instance",
}]
async fn instance_ensure(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    request: TypedBody<propolis_client::api::InstanceEnsureRequest>,
) -> Result<
    HttpResponseCreated<propolis_client::api::InstanceEnsureResponse>,
    HttpError,
> {
    let server_context = rqctx.context();
    let request = request.into_inner();

    // Handle requests to an instance that has already been initialized. Treat
    // the instances as compatible (and return Ok) if they have the same
    // properties and return an appropriate error otherwise.
    //
    // TODO(#205): Consider whether to use this interface to change an
    // instance's devices and backends at runtime.
    if let VmControllerState::Created(existing) =
        &*server_context.services.vm.lock().await
    {
        let existing_properties = existing.properties();
        if existing_properties.id != request.properties.id {
            return Err(HttpError::for_internal_error(format!(
                "Server already initialized with ID {}",
                existing_properties.id
            )));
        }

        if *existing_properties != request.properties {
            return Err(HttpError::for_internal_error(
                "Cannot update running server".to_string(),
            ));
        }

        return Ok(HttpResponseCreated(api::InstanceEnsureResponse {
            migrate: None,
        }));
    }

    let (instance_spec, in_memory_disk_contents) =
        instance_spec_from_request(&request, &server_context.static_config.vm)
            .map_err(|e| {
                HttpError::for_bad_request(
                    None,
                    format!(
                        "failed to generate instance spec from request: {}",
                        e
                    ),
                )
            })?;

    let producer_registry =
        if let Some(cfg) = server_context.static_config.metrics.as_ref() {
            Some(
                register_oximeter(
                    server_context,
                    cfg,
                    request.properties.id,
                    rqctx.log.clone(),
                )
                .await
                .map_err(|e| {
                    HttpError::for_internal_error(format!(
                        "failed to register to produce metrics: {}",
                        e
                    ))
                })?,
            )
        } else {
            None
        };

    let vm = VmController::new(
        instance_spec,
        request.properties,
        &server_context.static_config,
        in_memory_disk_contents,
        producer_registry,
        server_context.log.clone(),
        tokio::runtime::Handle::current(),
    )
    .map_err(|e| {
        HttpError::for_internal_error(format!(
            "failed to create instance: {}",
            e
        ))
    })?;

    if let Some(ramfb) = vm.framebuffer() {
        // Get a framebuffer description from the wrapped instance.
        let fb_spec = ramfb.get_framebuffer_spec();
        let vnc_fb = crate::vnc::RamFb::new(fb_spec);

        // Get a reference to the outward-facing VNC server in this process.
        let vnc_server_ref = server_context.services.vnc_server.clone();
        let vnc_server = vnc_server_ref.lock().await;

        // Give the Propolis VNC adapter a back pointer to the VNC server to
        // allow it to update pixel formats.
        //
        // N.B. Cloning `vnc_server` (the server guarded by the mutex) is
        //      correct here because the `rfb::VncServer` type is just a wrapper
        //      around the set of `Arc`s to the objecst needed to implement the
        //      server. In other words, creating a deep copy of `vnc_server`
        //      doesn't create a second server--it just creates a second set of
        //      references to the set of objects that the `rfb` crate uses to
        //      implement its behavior.
        vnc_server
            .server
            .initialize(vnc_fb, vm.instance().clone(), vnc_server.clone())
            .await;

        let notifier_server_ref = vnc_server_ref.clone();
        let rt = tokio::runtime::Handle::current();
        ramfb.set_notifier(Box::new(move |config, is_valid| {
            let h = notifier_server_ref.clone();
            rt.block_on(async move {
                let vnc = h.lock().await;
                vnc.server.update(config, is_valid).await;
            });
        }));
    }

    *server_context.services.vm.lock().await =
        VmControllerState::Created(vm.clone());

    let migrate = if let Some(migrate_request) = request.migrate {
        let res = crate::migrate::dest_initiate(&rqctx, vm, migrate_request)
            .await
            .map_err(<_ as Into<HttpError>>::into)?;
        Some(res)
    } else {
        None
    };

    Ok(HttpResponseCreated(propolis_client::api::InstanceEnsureResponse {
        migrate,
    }))
}

#[endpoint {
    method = GET,
    path = "/instance",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<propolis_client::api::InstanceGetResponse>, HttpError>
{
    let ctx = rqctx.context();
    let instance_info = match &*ctx.services.vm.lock().await {
        VmControllerState::NotCreated => {
            return Err(HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            ));
        }
        VmControllerState::Created(vm) => {
            propolis_client::api::Instance {
                properties: vm.properties().clone(),
                state: vm.external_instance_state(),
                disks: vec![],
                // TODO: Fix this; we need a way to enumerate attached NICs.
                // Possibly using the inventory of the instance?
                //
                // We *could* record whatever information about the NIC we want
                // when they're requested (adding fields to the server), but that
                // would make it difficult for Propolis to update any dynamic info
                // (i.e., has the device faulted, etc).
                nics: vec![],
            }
        }
        VmControllerState::Destroyed(inst) => inst.clone(),
    };

    Ok(HttpResponseOk(propolis_client::api::InstanceGetResponse {
        instance: instance_info,
    }))
}

#[endpoint {
    method = GET,
    path = "/instance/state-monitor",
}]
async fn instance_state_monitor(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceStateMonitorRequest>,
) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError> {
    let gen = request.into_inner().gen;
    let mut state_watcher = rqctx.context().vm().await?.state_watcher().clone();

    loop {
        let last = state_watcher.borrow().clone();
        if gen <= last.gen {
            return Ok(HttpResponseOk(last));
        }
        state_watcher.changed().await.unwrap();
    }
}

#[endpoint {
    method = PUT,
    path = "/instance/state",
}]
async fn instance_state_put(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceStateRequested>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let ctx = rqctx.context();
    let requested_state = request.into_inner();
    let vm = ctx.vm().await?;
    let result = vm
        .put_state(requested_state)
        .map(|_| HttpResponseUpdatedNoContent {})
        .map_err(|e| e.into());

    drop(vm);
    if result.is_ok() {
        match requested_state {
            api::InstanceStateRequested::Reboot => {
                let stats = ctx.services.oximeter_stats.lock().await;
                if let Some(stats) = stats.as_ref() {
                    stats.count_reset();
                }
            }
            api::InstanceStateRequested::Stop => {
                ctx.services.stop(&rqctx.log).await;
            }
            _ => {}
        }
    }

    result
}

#[endpoint {
    method = GET,
    path = "/instance/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
) -> Result<Response<Body>, HttpError> {
    let ctx = rqctx.context();
    let vm = ctx.vm().await?;
    let serial = vm.com1().clone();
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
        .map(handshake::derive_accept_key)
        .ok_or_else(|| {
            HttpError::for_bad_request(
                None,
                "missing websocket key".to_string(),
            )
        })?;

    let ws_log = rqctx.log.new(o!());
    let err_log = ws_log.clone();
    let mut serial_task = ctx.services.serial_task.lock().await;
    let serial_task = serial_task.get_or_insert_with(move || {
        let (websocks_ch, websocks_recv) = mpsc::channel(1);
        let (close_ch, close_recv) = oneshot::channel();

        let task = tokio::spawn(async move {
            if let Err(e) = super::serial::instance_serial_task(
                websocks_recv,
                close_recv,
                serial,
                ws_log.clone(),
            )
            .await
            {
                error!(ws_log, "Failed to spawn instance serial task: {}", e);
            }
        });

        super::serial::SerialTask {
            task,
            close_ch: Some(close_ch),
            websocks_ch,
        }
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
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceMigrateStartRequest>,
) -> Result<Response<Body>, HttpError> {
    let migration_id = request.into_inner().migration_id;
    crate::migrate::source_start(rqctx, migration_id).await.map_err(Into::into)
}

#[endpoint {
    method = GET,
    path = "/instance/migrate/status"
}]
async fn instance_migrate_status(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceMigrateStatusRequest>,
) -> Result<HttpResponseOk<api::InstanceMigrateStatusResponse>, HttpError> {
    let migration_id = request.into_inner().migration_id;
    let vm = rqctx.context().vm().await?;
    vm.migrate_status(migration_id).map_err(Into::into).map(|state| {
        HttpResponseOk(api::InstanceMigrateStatusResponse { state })
    })
}

/// Issues a snapshot request to a crucible backend.
#[endpoint {
    method = POST,
    path = "/instance/disk/{id}/snapshot/{snapshot_id}",
}]
async fn instance_issue_crucible_snapshot_request(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    path_params: Path<api::SnapshotRequestPathParams>,
) -> Result<HttpResponseOk<()>, HttpError> {
    let inst = rqctx.context().vm().await?;
    let crucible_backends = inst.crucible_backends();
    let path_params = path_params.into_inner();

    let backend = crucible_backends.get(&path_params.id).ok_or_else(|| {
        let s = format!("no disk with id {}!", path_params.id);
        HttpError::for_not_found(Some(s.clone()), s)
    })?;
    backend.snapshot(path_params.snapshot_id).map_err(|e| {
        HttpError::for_bad_request(Some(e.to_string()), e.to_string())
    })?;

    Ok(HttpResponseOk(()))
}

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<DropshotEndpointContext> {
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
