//! HTTP server callback functions.
//!
//! Functions in this module verify parameters and convert between types (API
//! request types to Propolis-native types and Propolis-native error types to
//! HTTP error codes) before sending operations to other components (e.g. the VM
//! controller) for processing.

use std::sync::Arc;
use std::{collections::BTreeMap, net::SocketAddr};

use dropshot::{
    channel, endpoint, ApiDescription, HttpError, HttpResponseCreated,
    HttpResponseOk, HttpResponseUpdatedNoContent, Path, RequestContext,
    TypedBody, WebsocketConnection,
};
use hyper::{Body, Response};
use oximeter::types::ProducerRegistry;
use propolis_client::{
    handmade::api,
    instance_spec::{self, InstanceSpec},
};
use propolis_server_config::Config as VmTomlConfig;
use rfb::server::VncServer;
use slog::{error, o, Logger};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, MappedMutexGuard, Mutex, MutexGuard};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use crate::spec::{ServerSpecBuilder, ServerSpecBuilderError};
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

    /// No VM controller exists.
    ///
    /// Distinguishing this state from `NotCreated` allows the server to discard
    /// the active `VmController` on instance stop while still being able to
    /// service get requests for the instance. (If this were not needed, or the
    /// server were willing to preserve the `VmController` after halt, this enum
    /// could be replaced with an `Option`.)
    Destroyed {
        /// A copy of the instance properties recorded at the time the instance
        /// was destroyed, used to serve subsequent `instance_get` requests.
        last_instance: api::Instance,

        /// A clone of the receiver side of the server's state watcher, used to
        /// serve subsequent `instance_state_monitor` requests. Note that an
        /// outgoing controller can publish new state changes even after the
        /// server has dropped its reference to it (its state worker may
        /// continue running for a time).
        watcher:
            tokio::sync::watch::Receiver<api::InstanceStateMonitorResponse>,
    },
}

impl VmControllerState {
    /// Maps this `VmControllerState` into a mutable reference to its internal
    /// `VmController` if a controller is active.
    pub fn as_controller(&mut self) -> Option<&mut Arc<VmController>> {
        match self {
            VmControllerState::NotCreated => None,
            VmControllerState::Created(c) => Some(c),
            VmControllerState::Destroyed { .. } => None,
        }
    }

    /// Takes the active `VmController` if one is present and replaces it with
    /// `VmControllerState::Destroyed`.
    pub fn take_controller(&mut self) -> Option<Arc<VmController>> {
        if let VmControllerState::Created(vm) = self {
            let last_instance = api::Instance {
                properties: vm.properties().clone(),
                state: api::InstanceState::Destroyed,
                disks: vec![],
                nics: vec![],
            };

            // The server is about to drop its reference to the controller, but
            // the controller may continue changing state while it tears itself
            // down. Grab a clone of the state watcher channel for subsequent
            // calls to `instance_state_monitor` to use.
            let watcher = vm.state_watcher().clone();
            if let VmControllerState::Created(vm) = std::mem::replace(
                self,
                VmControllerState::Destroyed { last_instance, watcher },
            ) {
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

    /// The currently active serial console handling task, if present.
    serial_task: Mutex<Option<super::serial::SerialTask>>,

    /// The host task for this Propolis server's Oximeter server.
    oximeter_server_task: Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// The metrics wrapper for "server-level" metrics, i.e., metrics that are
    /// tracked by the server itself (as opposed to being tracked by a component
    /// within an instance).
    oximeter_stats: Mutex<Option<crate::stats::ServerStatsOuter>>,

    /// The VNC server hosted within this process. Note that this server always
    /// exists irrespective of whether there is an instance. Creating an
    /// instance hooks this server up to the instance's framebuffer.
    vnc_server: Arc<Mutex<VncServer<PropolisVncServer>>>,
}

impl ServiceProviders {
    /// Directs the current set of per-instance service providers to stop in an
    /// orderly fashion, then drops them all.
    async fn stop(&self, log: &Logger) {
        if let Some(vm) = self.vm.lock().await.take_controller() {
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
    pub services: Arc<ServiceProviders>,
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
            services: Arc::new(ServiceProviders {
                vm: Mutex::new(VmControllerState::NotCreated),
                serial_task: Mutex::new(None),
                oximeter_server_task: Mutex::new(None),
                oximeter_stats: Mutex::new(None),
                vnc_server: Arc::new(Mutex::new(vnc_server)),
            }),
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
            VmControllerState::as_controller,
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
    SpecBuilderError(#[from] ServerSpecBuilderError),
}

/// Creates an instance spec from an ensure request. (Both types are foreign to
/// this crate, so implementing TryFrom for them is not allowed.)
fn instance_spec_from_request(
    request: &api::InstanceEnsureRequest,
    toml_config: &VmTomlConfig,
) -> Result<InstanceSpec, SpecCreationError> {
    let mut spec_builder =
        ServerSpecBuilder::new(&request.properties, toml_config)?;

    for nic in &request.nics {
        spec_builder.add_nic_from_request(nic)?;
    }

    for disk in &request.disks {
        spec_builder.add_disk_from_request(disk)?;
    }

    if let Some(base64) = &request.cloud_init_bytes {
        spec_builder.add_cloud_init_from_request(base64.clone())?;
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

    Ok(spec_builder.finish())
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
    request: TypedBody<api::InstanceEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
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

    let instance_spec =
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

    let (stop_ch, stop_recv) = oneshot::channel();

    // Parts of VM initialization (namely Crucible volume attachment) make use
    // of async processing, which itself is turned synchronous with `block_on`
    // calls to the Tokio runtime.
    //
    // Since `block_on` will panic if called from an async context, as we are in
    // now, the whole process is wrapped up in `spawn_blocking`.  It is
    // admittedly a big kludge until this can be better refactored.
    let vm = {
        let properties = request.properties.clone();
        let use_reservoir = server_context.static_config.use_reservoir;
        let bootrom = server_context.static_config.vm.bootrom.clone();
        let log = server_context.log.clone();
        let hdl = tokio::runtime::Handle::current();
        let ctrl_hdl = hdl.clone();

        let vm_hdl = hdl.spawn_blocking(move || {
            VmController::new(
                instance_spec,
                properties,
                use_reservoir,
                bootrom,
                producer_registry,
                log,
                ctrl_hdl,
                stop_ch,
            )
        });

        vm_hdl.await.unwrap()
    }
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

        // Get a reference to the PS2 controller so that we can pass keyboard input.
        let ps2ctrl = vm.ps2ctrl().unwrap();

        // Get a reference to the outward-facing VNC server in this process.
        let vnc_server_ref = server_context.services.vnc_server.clone();
        let vnc_server = vnc_server_ref.lock().await;

        // Give the Propolis VNC adapter a back pointer to the VNC server to
        // allow them to communicate bidirectionally: the VNC server pulls data
        // from the Propolis adapter using the adapter's `rfb::server` impl, and
        // the Propolis adapter pushes data (e.g. pixel format and resolution
        // changes) to the VNC server using the adapter's public interface.
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
            .initialize(
                vnc_fb,
                Arc::clone(ps2ctrl),
                vm.instance().clone(),
                vnc_server.clone(),
            )
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

    let log = server_context.log.clone();
    let services = Arc::clone(&server_context.services);
    tokio::task::spawn(async move {
        // Once the VmController has signaled that it is shutting down,
        // we'll clean up the per-instance service providers as well.
        let _ = stop_recv.await;
        services.stop(&log).await;
    });

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

    Ok(HttpResponseCreated(api::InstanceEnsureResponse { migrate }))
}

#[endpoint {
    method = GET,
    path = "/instance",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    let ctx = rqctx.context();
    let instance_info = match &*ctx.services.vm.lock().await {
        VmControllerState::NotCreated => {
            return Err(HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            ));
        }
        VmControllerState::Created(vm) => {
            api::Instance {
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
        VmControllerState::Destroyed { last_instance, .. } => {
            last_instance.clone()
        }
    };

    Ok(HttpResponseOk(api::InstanceGetResponse { instance: instance_info }))
}

#[endpoint {
    method = GET,
    path = "/instance/state-monitor",
}]
async fn instance_state_monitor(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceStateMonitorRequest>,
) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError> {
    let ctx = rqctx.context();
    let gen = request.into_inner().gen;
    let mut state_watcher = {
        // N.B. This lock must be dropped before entering the loop below.
        let vm_state = ctx.services.vm.lock().await;
        match &*vm_state {
            VmControllerState::NotCreated => {
                return Err(HttpError::for_internal_error(
                    "Server not initialized (no instance)".to_string(),
                ));
            }
            VmControllerState::Created(vm) => vm.state_watcher().clone(),
            VmControllerState::Destroyed { watcher, .. } => watcher.clone(),
        }
    };

    loop {
        let last = state_watcher.borrow().clone();
        if gen <= last.gen {
            return Ok(HttpResponseOk(last));
        }

        // An error from `changed` indicates that the sender was destroyed,
        // which means that the generation number will never change again, which
        // means it will never reach the number the client wants it to reach.
        // Inform the client of this condition so it doesn't wait forever.
        state_watcher.changed().await.map_err(|_| {
            HttpError::for_unavail(
                None,
                format!(
                    "No instance present; will never reach generation {}",
                    gen
                ),
            )
        })?;
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
            _ => {}
        }
    }

    result
}

#[channel {
    protocol = WEBSOCKETS,
    path = "/instance/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<DropshotEndpointContext>>,
    websock: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let ctx = rqctx.context();
    let vm = ctx.vm().await?;
    let serial = vm.com1().clone();

    let err_log = rqctx.log.new(o!());

    // Create or get active serial task handle and channels
    let mut serial_task = ctx.services.serial_task.lock().await;
    let serial_task = serial_task.get_or_insert_with(move || {
        let (websocks_ch, websocks_recv) = mpsc::channel(1);
        let (close_ch, close_recv) = oneshot::channel();

        let task = tokio::spawn(async move {
            if let Err(e) = super::serial::instance_serial_task(
                websocks_recv,
                close_recv,
                serial,
                err_log.clone(),
            )
            .await
            {
                error!(err_log, "Failed to spawn instance serial task: {}", e);
            }
        });

        super::serial::SerialTask {
            task,
            close_ch: Some(close_ch),
            websocks_ch,
        }
    });

    let config =
        WebSocketConfig { max_send_queue: Some(4096), ..Default::default() };
    let websocks_send = serial_task.websocks_ch.clone();

    let ws_stream = WebSocketStream::from_raw_socket(
        websock.into_inner(),
        Role::Server,
        Some(config),
    )
    .await;

    websocks_send
        .send(ws_stream.into())
        .await
        .map_err(|e| format!("Serial socket hand-off failed: {}", e).into())
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_propolis_server_openapi() {
        let mut buf: Vec<u8> = vec![];
        super::api()
            .openapi("Oxide Propolis Server API", "0.0.1")
            .description(
                "API for interacting with the Propolis hypervisor frontend.",
            )
            .contact_url("https://oxide.computer")
            .contact_email("api@oxide.computer")
            .write(&mut buf)
            .unwrap();
        let output = String::from_utf8(buf).unwrap();
        expectorate::assert_contents(
            "../../openapi/propolis-server.json",
            &output,
        );
    }
}
