// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP server callback functions.
//!
//! Functions in this module verify parameters and convert between types (API
//! request types to Propolis-native types and Propolis-native error types to
//! HTTP error codes) before sending operations to other components (e.g. the VM
//! controller) for processing.

use std::convert::TryFrom;
use std::net::Ipv6Addr;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::{collections::BTreeMap, net::SocketAddr};

use crate::migrate::MigrateError;
use crate::serial::history_buffer::SerialHistoryOffset;
use crate::serial::SerialTaskControlMessage;
use dropshot::{
    channel, endpoint, ApiDescription, HttpError, HttpResponseCreated,
    HttpResponseOk, HttpResponseUpdatedNoContent, Path, Query, RequestContext,
    TypedBody, WebsocketConnection,
};
use futures::SinkExt;
use internal_dns::resolver::{ResolveError, Resolver};
use internal_dns::ServiceName;
pub use nexus_client::Client as NexusClient;
use oximeter::types::ProducerRegistry;
use propolis_api_types as api;
use propolis_api_types::instance_spec::{
    self, components::backends::CrucibleStorageBackend, v0::StorageBackendV0,
    VersionedInstanceSpec,
};

use propolis_server_config::Config as VmTomlConfig;
use rfb::server::VncServer;
use slog::{error, info, o, warn, Logger};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, MappedMutexGuard, Mutex, MutexGuard};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use crate::spec::{ServerSpecBuilder, ServerSpecBuilderError};
use crate::stats::virtual_machine::VirtualMachine;
use crate::vm::VmController;
use crate::vnc::PropolisVncServer;

pub(crate) type DeviceMap =
    BTreeMap<String, Arc<dyn propolis::common::Lifecycle>>;
pub(crate) type BlockBackendMap =
    BTreeMap<String, Arc<dyn propolis::block::Backend>>;
pub(crate) type CrucibleBackendMap =
    BTreeMap<uuid::Uuid, Arc<propolis::block::CrucibleBackend>>;

/// Configuration used to set this server up to provide Oximeter metrics.
#[derive(Debug, Clone)]
pub struct MetricsEndpointConfig {
    /// The address at which the Oximeter endpoint will be hosted (i.e., this
    /// server's address).
    pub propolis_addr: SocketAddr,

    /// The address of the Nexus instance with which we should register our own
    /// server's address.
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
        last_instance: Box<api::Instance>,

        /// A copy of the destroyed instance's spec, used to serve subsequent
        /// `instance_spec_get` requests.
        //
        // TODO: Merge this into `api::Instance` when the migration to generated
        // types is complete.
        last_instance_spec: Box<VersionedInstanceSpec>,

        /// A clone of the receiver side of the server's state watcher, used to
        /// serve subsequent `instance_state_monitor` requests. Note that an
        /// outgoing controller can publish new state changes even after the
        /// server has dropped its reference to it (its state worker may
        /// continue running for a time).
        state_watcher:
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
    pub async fn take_controller(&mut self) -> Option<Arc<VmController>> {
        if let VmControllerState::Created(vm) = self {
            let state = vm.state_watcher().borrow().state;
            let last_instance = api::Instance {
                properties: vm.properties().clone(),
                state,
                disks: vec![],
                nics: vec![],
            };
            let last_instance_spec = vm.instance_spec().await.clone();

            // Preserve the state watcher so that subsequent updates to the VM's
            // state are visible to calls to query/monitor that state. Note that
            // the VM's state will change at least once more after this point:
            // the final transition to the "destroyed" state happens only when
            // all references to the VM have been dropped, including the one
            // this routine just exchanged and will return.
            let state_watcher = vm.state_watcher().clone();
            if let VmControllerState::Created(vm) = std::mem::replace(
                self,
                VmControllerState::Destroyed {
                    last_instance: Box::new(last_instance),
                    last_instance_spec: Box::new(last_instance_spec),
                    state_watcher,
                },
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

/// Objects related to Propolis's Oximeter metric production.
pub struct OximeterState {
    /// A task spawned at initial startup to register with Nexus for metric
    /// production.
    ///
    /// The oneshot sender will be used in the `ServiceProviders::stop()`
    /// method, to signal that the registration task itself should bail out, if
    /// it's still running.
    registration_task:
        Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,

    /// The host task for this Propolis server's Oximeter server.
    server_task: Option<tokio::task::JoinHandle<()>>,

    /// The metrics wrapper for "server-level" metrics, i.e., metrics that are
    /// tracked by the server itself (as opposed to being tracked by a component
    /// within an instance).
    stats: Option<crate::stats::ServerStatsOuter>,
}

/// Objects that this server creates, owns, and manipulates in response to API
/// calls.
pub struct ServiceProviders {
    /// The VM controller that manages this server's Propolis instance. This is
    /// `None` until a guest is created via `instance_ensure`.
    pub vm: Mutex<VmControllerState>,

    /// The currently active serial console handling task, if present.
    serial_task: Mutex<Option<super::serial::SerialTask>>,

    /// State related to the Propolis Oximeter server, registration task, and
    /// actual statistics.
    oximeter_state: Mutex<OximeterState>,

    /// The VNC server hosted within this process. Note that this server always
    /// exists irrespective of whether there is an instance. Creating an
    /// instance hooks this server up to the instance's framebuffer.
    vnc_server: Arc<VncServer<PropolisVncServer>>,
}

impl ServiceProviders {
    /// Directs the current set of per-instance service providers to stop in an
    /// orderly fashion, then drops them all.
    async fn stop(&self, log: &Logger) {
        // Stop the VNC server
        self.vnc_server.stop().await;

        if let Some(vm) = self.vm.lock().await.take_controller().await {
            slog::info!(log, "Dropping server's VM controller reference";
                "strong_refs" => Arc::strong_count(&vm),
                "weak_refs" => Arc::weak_count(&vm),
            );
        }
        if let Some(serial_task) = self.serial_task.lock().await.take() {
            let _ = serial_task
                .control_ch
                .send(SerialTaskControlMessage::Stopping)
                .await;
            // Wait for the serial task to exit
            let _ = serial_task.task.await;
        }

        // Clean up oximeter tasks and statistic state.
        let mut oximeter_state = self.oximeter_state.lock().await;
        if let Some((shutdown_tx, task)) =
            oximeter_state.registration_task.take()
        {
            let _ = shutdown_tx.send(());
            task.abort();
        };
        if let Some(server) = oximeter_state.server_task.take() {
            server.abort();
        }
        let _ = oximeter_state.stats.take();
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
        vnc_server: Arc<VncServer<PropolisVncServer>>,
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
                oximeter_state: Mutex::new(OximeterState {
                    registration_task: None,
                    server_task: None,
                    stats: None,
                }),
                vnc_server,
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
        .map_err(|_| not_created_error())
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
) -> Result<VersionedInstanceSpec, SpecCreationError> {
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
        instance_spec::components::devices::SerialPortNumber::Com1,
        instance_spec::components::devices::SerialPortNumber::Com2,
        instance_spec::components::devices::SerialPortNumber::Com3,
        // SoftNpu uses this port for ASIC management.
        #[cfg(not(feature = "falcon"))]
        instance_spec::components::devices::SerialPortNumber::Com4,
    ] {
        spec_builder.add_serial_port(port)?;
    }

    Ok(VersionedInstanceSpec::V0(spec_builder.finish()))
}

/// Register an Oximeter server reporting metrics from a new instance.
///
/// This spawns a tokio task which will indefinitely attempt to register with
/// Nexus as an Oximeter metric producer. Once the registration succeeds, it
/// will also arrange for the statistics produced for the managed instance to be
/// collected.
async fn register_oximeter_in_background(
    services: Arc<ServiceProviders>,
    cfg: MetricsEndpointConfig,
    registry: ProducerRegistry,
    virtual_machine: VirtualMachine,
    log: Logger,
) {
    let mut oximeter_state = services.oximeter_state.lock().await;
    assert!(oximeter_state.stats.is_none());
    assert!(oximeter_state.server_task.is_none());
    assert!(oximeter_state.registration_task.is_none());

    // Start a task which attempts to register with Nexus as a metric producer.
    //
    // This task will loop until either (1) registration succeeds, (2) a
    // permanent error occurs during registration, or (3) it is notified by the
    // shutdown procedure in `ServiceProviders::stop()`. In the former case,
    // we'll return the `oximeter` server and continue registration. In the
    // latter cases, we'll return early, since no metrics can be produced.
    let services_ = services.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let registration_task = tokio::task::spawn(async move {
        // Create the closure used to register with Nexus inside the backoff
        // loop.
        let register_with_nexus = || async {
            crate::stats::start_oximeter_server(
                virtual_machine.instance_id,
                &cfg,
                &log,
                &registry,
            )
            .await
            .map_err(|e| match e {
                oximeter_producer::Error::RegistrationError {
                    retryable,
                    msg,
                } if retryable => {
                    omicron_common::backoff::BackoffError::transient(msg)
                }
                _ => omicron_common::backoff::BackoffError::permanent(
                    e.to_string(),
                ),
            })
        };
        let warn_on_failure = |error, delay| {
            warn!(
                log,
                "failed to register as a metric producer with Nexus";
                "error" => ?error,
                "retry_after" => ?delay,
            );
        };
        let retry_fut = omicron_common::backoff::retry_notify(
            omicron_common::backoff::retry_policy_internal_service(),
            register_with_nexus,
            warn_on_failure,
        );

        // Select over the retry loop itself, or the notification from the
        // `ServiceProviders` shutdown method that we need to bail.
        let server = tokio::select! {
            res = retry_fut => {
                match res {
                    Ok(server) => {
                        info!(log, "registered as a metric producer with Nexus");
                        server
                    }
                    Err(e) => {
                        error!(
                            log,
                            "encountered non-retryable error when starting \
                            oximeter metric production server, no metrics will \
                            be produced for this instance";
                            "error" => ?e,
                        );
                        return;
                    }
                }
            }
            _ = shutdown_rx => {
                slog::debug!(
                    log,
                    "Nexus metric registration retry loop \
                    received shutdown notification, exiting"
                );
                return;
            }
        };

        // Actually spawn the server task and store its handle.
        let server_task = tokio::spawn(async move {
            server.serve_forever().await.unwrap();
        });
        let mut state = services_.oximeter_state.lock().await;
        let old = state.server_task.replace(server_task);
        assert!(old.is_none());

        // Assign our own metrics production for this VM instance to the
        // registry, letting the server actually return them to oximeter when
        // polled.
        let stats = match crate::stats::register_server_metrics(
            &registry,
            virtual_machine,
            &log,
        )
        .await
        {
            Ok(stats) => stats,
            Err(e) => {
                error!(
                    log,
                    "failed to register our server metrics with \
                    the ProducerRegistry, no server stats will \
                    be produced";
                    "error" => ?e,
                );
                return;
            }
        };
        let old = state.stats.replace(stats);
        assert!(old.is_none());
    });
    let old = oximeter_state
        .registration_task
        .replace((shutdown_tx, registration_task));
    assert!(old.is_none());
}

/// Wrapper around a [`NexusClient`] object, which allows deferring
/// the DNS lookup until accessed.
///
/// Without the assistance of OS-level DNS lookups, the [`NexusClient`]
/// interface requires knowledge of the target service IP address.
/// For some services, like Nexus, this can be painful, as the IP address
/// may not have even been allocated when the Sled Agent starts.
///
/// This structure allows clients to access the client on-demand, performing
/// the DNS lookup only once it is actually needed.
struct LazyNexusClientInner {
    log: Logger,
    resolver: Resolver,
}
#[derive(Clone)]
pub struct LazyNexusClient {
    inner: Arc<LazyNexusClientInner>,
}

impl LazyNexusClient {
    pub fn new(log: Logger, addr: Ipv6Addr) -> Result<Self, ResolveError> {
        Ok(Self {
            inner: Arc::new(LazyNexusClientInner {
                log: log.clone(),
                resolver: Resolver::new_from_ip(log, addr)?,
            }),
        })
    }

    pub async fn get_ip(&self) -> Result<SocketAddrV6, ResolveError> {
        self.inner.resolver.lookup_socket_v6(ServiceName::Nexus).await
    }

    pub async fn get(&self) -> Result<NexusClient, ResolveError> {
        let address = self.get_ip().await?;

        Ok(NexusClient::new(
            &format!("http://{}", address),
            self.inner.log.clone(),
        ))
    }
}

// Use our local address as basis for calculating a Nexus endpoint,
// Return that endpoint if successful.
async fn find_local_nexus_client(
    local_addr: SocketAddr,
    log: Logger,
) -> Option<NexusClient> {
    // At the moment, we only support converting an IPv6 address into a
    // Nexus endpoint.
    let address = match local_addr {
        SocketAddr::V6(my_address) => *my_address.ip(),
        SocketAddr::V4(_) => {
            warn!(log, "Unable to determine Nexus endpoint for IPv4 addresses");
            return None;
        }
    };

    // We have an IPv6 address, so could be in a rack.  See if there is a
    // Nexus at the expected location.
    match LazyNexusClient::new(log.clone(), address) {
        Ok(lnc) => match lnc.get().await {
            Ok(client) => Some(client),
            Err(e) => {
                warn!(log, "Failed to determine Nexus: endpoint {}", e);
                None
            }
        },
        Err(e) => {
            warn!(log, "Failed to get Nexus client: {}", e);
            None
        }
    }
}

async fn instance_ensure_common(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    request: api::InstanceSpecEnsureRequest,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    let server_context = rqctx.context();
    let api::InstanceSpecEnsureRequest { properties, instance_spec, migrate } =
        request;

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
        if existing_properties.id != properties.id {
            return Err(HttpError::for_client_error(
                Some(api::ErrorCode::AlreadyInitialized.to_string()),
                http::status::StatusCode::CONFLICT,
                format!(
                    "Server already initialized with ID {}",
                    existing_properties.id
                ),
            ));
        }

        if *existing_properties != properties {
            return Err(HttpError::for_client_error(
                Some(api::ErrorCode::AlreadyRunning.to_string()),
                http::status::StatusCode::CONFLICT,
                "Cannot update running server".to_string(),
            ));
        }

        return Ok(HttpResponseCreated(api::InstanceEnsureResponse {
            migrate: None,
        }));
    }

    let producer_registry =
        if let Some(cfg) = server_context.static_config.metrics.as_ref() {
            // Create a registry and spawn tasks to register with Nexus as an
            // oximeter metric producer.
            //
            // We create a registry here so that we can pass it through to Crucible
            // below. We also spawn a task for the actual registration process
            // (which may spin indefinitely) so that we can continue to initialize
            // the VM instance without blocking for that to succeed.
            let registry = ProducerRegistry::with_id(properties.id);
            let virtual_machine = VirtualMachine::from(&properties);
            register_oximeter_in_background(
                server_context.services.clone(),
                cfg.clone(),
                registry.clone(),
                virtual_machine,
                rqctx.log.clone(),
            )
            .await;
            Some(registry)
        } else {
            None
        };

    let (stop_ch, stop_recv) = oneshot::channel();

    // Use our current address to generate the expected Nexus client endpoint
    // address.
    let nexus_client =
        find_local_nexus_client(rqctx.server.local_addr, rqctx.log.clone())
            .await;

    // Parts of VM initialization (namely Crucible volume attachment) make use
    // of async processing, which itself is turned synchronous with `block_on`
    // calls to the Tokio runtime.
    //
    // Since `block_on` will panic if called from an async context, as we are in
    // now, the whole process is wrapped up in `spawn_blocking`.  It is
    // admittedly a big kludge until this can be better refactored.
    let vm = {
        let properties = properties.clone();
        let server_context = server_context.clone();
        let log = server_context.log.clone();
        let hdl = tokio::runtime::Handle::current();
        let ctrl_hdl = hdl.clone();
        let vm_hdl = hdl.spawn_blocking(move || {
            VmController::new(
                instance_spec,
                properties,
                &server_context.static_config,
                producer_registry,
                nexus_client,
                log,
                ctrl_hdl,
                stop_ch,
            )
        });

        vm_hdl.await.unwrap()
    }
    .map_err(|e| {
        HttpError::for_client_error(
            Some(api::ErrorCode::CreateFailed.to_string()),
            http::status::StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create instance: {e}"),
        )
    })?;

    if let Some(ramfb) = vm.framebuffer() {
        // Get a framebuffer description from the wrapped instance.
        let fb_spec = ramfb.get_framebuffer_spec();
        let vnc_fb = crate::vnc::RamFb::new(fb_spec);

        // Get a reference to the PS2 controller so that we can pass keyboard input.
        let ps2ctrl = vm.ps2ctrl().clone();

        // Get a reference to the outward-facing VNC server in this process.
        let vnc_server = server_context.services.vnc_server.clone();

        // Initialize the Propolis VNC adapter with references to the VM's Instance,
        // framebuffer, and PS2 controller.
        vnc_server.server.initialize(vnc_fb, ps2ctrl, vm.clone()).await;

        // Hook up the framebuffer notifier to update the Propolis VNC adapter
        let notifier_server_ref = vnc_server.clone();
        let rt = tokio::runtime::Handle::current();
        ramfb.set_notifier(Box::new(move |config, is_valid| {
            let vnc = notifier_server_ref.clone();
            rt.block_on(vnc.server.update(config, is_valid, &vnc));
        }));
    }

    let mut serial_task = server_context.services.serial_task.lock().await;
    if serial_task.is_none() {
        let (websocks_ch, websocks_recv) = mpsc::channel(1);
        let (control_ch, control_recv) = mpsc::channel(1);

        let serial = vm.com1().clone();
        serial.set_task_control_sender(control_ch.clone()).await;
        let err_log = rqctx.log.new(o!("component" => "serial task"));
        let task = tokio::spawn(async move {
            if let Err(e) = super::serial::instance_serial_task(
                websocks_recv,
                control_recv,
                serial,
                err_log.clone(),
            )
            .await
            {
                error!(err_log, "Failure in serial task: {}", e);
            }
        });
        *serial_task =
            Some(super::serial::SerialTask { task, control_ch, websocks_ch });
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

    let migrate = if let Some(migrate_request) = migrate {
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
    method = PUT,
    path = "/instance",
}]
async fn instance_ensure(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    let server_context = rqctx.context();
    let request = request.into_inner();
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

    instance_ensure_common(
        rqctx,
        api::InstanceSpecEnsureRequest {
            properties: request.properties,
            instance_spec,
            migrate: request.migrate,
        },
    )
    .await
}

#[endpoint {
    method = PUT,
    path = "/instance/spec",
}]
async fn instance_spec_ensure(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceSpecEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    instance_ensure_common(rqctx, request.into_inner()).await
}

async fn instance_get_common(
    rqctx: &RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<(api::Instance, VersionedInstanceSpec), HttpError> {
    let ctx = rqctx.context();
    match &*ctx.services.vm.lock().await {
        VmControllerState::NotCreated => Err(not_created_error()),
        VmControllerState::Created(vm) => {
            Ok((
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
                },
                vm.instance_spec().await.clone(),
            ))
        }
        VmControllerState::Destroyed {
            last_instance,
            last_instance_spec,
            state_watcher,
            ..
        } => {
            let watcher = state_watcher.borrow();
            let mut last_instance = last_instance.clone();
            last_instance.state = watcher.state;
            Ok((*last_instance, *last_instance_spec.clone()))
        }
    }
}

#[endpoint {
    method = GET,
    path = "/instance/spec",
}]
async fn instance_spec_get(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<api::InstanceSpecGetResponse>, HttpError> {
    let (instance, spec) = instance_get_common(&rqctx).await?;
    Ok(HttpResponseOk(api::InstanceSpecGetResponse {
        properties: instance.properties,
        state: instance.state,
        spec,
    }))
}

#[endpoint {
    method = GET,
    path = "/instance",
}]
async fn instance_get(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    let (instance, _) = instance_get_common(&rqctx).await?;
    Ok(HttpResponseOk(api::InstanceGetResponse { instance }))
}

#[endpoint {
    method = GET,
    path = "/instance/state-monitor",
}]
async fn instance_state_monitor(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    request: TypedBody<api::InstanceStateMonitorRequest>,
) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError> {
    let ctx = rqctx.context();
    let gen = request.into_inner().gen;
    let mut state_watcher = {
        // N.B. This lock must be dropped before entering the loop below.
        let vm_state = ctx.services.vm.lock().await;
        match &*vm_state {
            VmControllerState::NotCreated => {
                return Err(not_created_error());
            }
            VmControllerState::Created(vm) => vm.state_watcher().clone(),
            VmControllerState::Destroyed { state_watcher, .. } => {
                state_watcher.clone()
            }
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
            HttpError::for_client_error(
                Some(api::ErrorCode::NoInstance.to_string()),
                http::status::StatusCode::GONE,
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
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
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
        if let api::InstanceStateRequested::Reboot = requested_state {
            let stats = MutexGuard::map(
                ctx.services.oximeter_state.lock().await,
                |state| &mut state.stats,
            );
            if let Some(stats) = stats.as_ref() {
                stats.count_reset();
            }
        }
    }

    result
}

#[endpoint {
    method = GET,
    path = "/instance/serial/history",
}]
async fn instance_serial_history_get(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    query: Query<api::InstanceSerialConsoleHistoryRequest>,
) -> Result<HttpResponseOk<api::InstanceSerialConsoleHistoryResponse>, HttpError>
{
    let ctx = rqctx.context();
    let vm = ctx.vm().await?;
    let serial = vm.com1().clone();
    let query_params = query.into_inner();

    let byte_offset = SerialHistoryOffset::try_from(&query_params)?;

    let max_bytes = query_params.max_bytes.map(|x| x as usize);
    let (data, end) = serial
        .history_vec(byte_offset, max_bytes)
        .await
        .map_err(|e| HttpError::for_bad_request(None, e.to_string()))?;

    Ok(HttpResponseOk(api::InstanceSerialConsoleHistoryResponse {
        data,
        last_byte_offset: end as u64,
    }))
}

#[channel {
    protocol = WEBSOCKETS,
    path = "/instance/serial",
}]
async fn instance_serial(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    query: Query<api::InstanceSerialConsoleStreamRequest>,
    websock: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let ctx = rqctx.context();
    let vm = ctx.vm().await?;
    let serial = vm.com1().clone();

    // Use the default buffering paramters for the websocket configuration
    //
    // Because messages are written with [`StreamExt::send`], the buffer on the
    // websocket is flushed for every message, preventing both unecessary delays
    // of messages and the potential for the buffer to grow without bound.
    let config = WebSocketConfig::default();

    let mut ws_stream = WebSocketStream::from_raw_socket(
        websock.into_inner(),
        Role::Server,
        Some(config),
    )
    .await;

    let byte_offset = SerialHistoryOffset::try_from(&query.into_inner()).ok();
    if let Some(mut byte_offset) = byte_offset {
        loop {
            let (data, offset) = serial.history_vec(byte_offset, None).await?;
            if data.is_empty() {
                break;
            }
            ws_stream
                .send(tokio_tungstenite::tungstenite::Message::Binary(data))
                .await?;
            byte_offset = SerialHistoryOffset::FromStart(offset);
        }
    }

    // Get serial task's handle and send it the websocket stream
    ctx.services
        .serial_task
        .lock()
        .await
        .as_ref()
        .ok_or("Instance has no serial task")?
        .websocks_ch
        .send(ws_stream)
        .await
        .map_err(|e| format!("Serial socket hand-off failed: {}", e).into())
}

// This endpoint is meant to only be called during a migration from the destination
// instance to the source instance as part of the HTTP connection upgrade used to
// establish the migration link. We don't actually want this exported via OpenAPI
// clients.
#[channel {
    protocol = WEBSOCKETS,
    path = "/instance/migrate/{migration_id}/start",
    unpublished = true,
}]
async fn instance_migrate_start(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    path_params: Path<api::InstanceMigrateStartRequest>,
    websock: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let migration_id = path_params.into_inner().migration_id;
    let conn = WebSocketStream::from_raw_socket(
        websock.into_inner(),
        Role::Server,
        None,
    )
    .await;
    crate::migrate::source_start(rqctx, migration_id, conn).await?;
    Ok(())
}

#[endpoint {
    method = GET,
    path = "/instance/migrate/{migration_id}/status"
}]
async fn instance_migrate_status(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    path_params: Path<api::InstanceMigrateStatusRequest>,
) -> Result<HttpResponseOk<api::InstanceMigrateStatusResponse>, HttpError> {
    let migration_id = path_params.into_inner().migration_id;
    let ctx = rqctx.context();
    match &*ctx.services.vm.lock().await {
        VmControllerState::NotCreated => Err(not_created_error()),
        VmControllerState::Created(vm) => {
            vm.migrate_status(migration_id).map_err(Into::into).map(|state| {
                HttpResponseOk(api::InstanceMigrateStatusResponse {
                    migration_id,
                    state,
                })
            })
        }
        VmControllerState::Destroyed { state_watcher, .. } => {
            let watcher = state_watcher.borrow();
            match &watcher.migration {
                None => Err((MigrateError::NoMigrationInProgress).into()),
                Some(migration_status)
                    if migration_status.migration_id == migration_id =>
                {
                    Ok(HttpResponseOk(migration_status.clone()))
                }
                Some(_) => Err((MigrateError::UuidMismatch).into()),
            }
        }
    }
}

/// Issues a snapshot request to a crucible backend.
#[endpoint {
    method = POST,
    path = "/instance/disk/{id}/snapshot/{snapshot_id}",
}]
async fn instance_issue_crucible_snapshot_request(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    path_params: Path<api::SnapshotRequestPathParams>,
) -> Result<HttpResponseOk<()>, HttpError> {
    let inst = rqctx.context().vm().await?;
    let crucible_backends = inst.crucible_backends();
    let path_params = path_params.into_inner();

    let backend = crucible_backends.get(&path_params.id).ok_or_else(|| {
        let s = format!("no disk with id {}!", path_params.id);
        HttpError::for_not_found(Some(s.clone()), s)
    })?;
    backend.snapshot(path_params.snapshot_id).await.map_err(|e| {
        HttpError::for_bad_request(Some(e.to_string()), e.to_string())
    })?;

    Ok(HttpResponseOk(()))
}

/// Issues a volume_construction_request replace to a crucible backend.
#[endpoint {
    method = PUT,
    path = "/instance/disk/{id}/vcr",
}]
async fn instance_issue_crucible_vcr_request(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    path_params: Path<api::VCRRequestPathParams>,
    request: TypedBody<api::InstanceVCRReplace>,
) -> Result<HttpResponseOk<()>, HttpError> {
    let path_params = path_params.into_inner();
    let request = request.into_inner();
    let new_vcr_json = request.vcr_json;
    let disk_name = request.name;
    let log = rqctx.log.clone();

    // Get the instance spec for storage backend from the disk name.  We use
    // the VCR stored there to send to crucible along with the new VCR we want
    // to replace it.
    let vm_controller = rqctx.context().vm().await?;

    // TODO(#205): Mutating a VM's configuration should be a first-class
    // operation in the VM controller that synchronizes with ongoing migrations
    // and other attempts to mutate the VM. For the time being, use the instance
    // spec lock to exclude other concurrent attempts to reconfigure this
    // backend.
    let mut spec = vm_controller.instance_spec().await;
    let VersionedInstanceSpec::V0(v0_spec) = &mut *spec;

    let (readonly, old_vcr_json) = {
        let bes = &v0_spec.backends.storage_backends.get(&disk_name);
        if let Some(StorageBackendV0::Crucible(bes)) = bes {
            (bes.readonly, &bes.request_json)
        } else {
            let s = format!("Crucible backend for {:?} not found", disk_name);
            return Err(HttpError::for_not_found(Some(s.clone()), s));
        }
    };

    // Get the crucible backend so we can call the replacement method on it.
    let crucible_backends = vm_controller.crucible_backends();
    let backend = crucible_backends.get(&path_params.id).ok_or_else(|| {
        let s = format!("No crucible backend for id {}", path_params.id);
        HttpError::for_not_found(Some(s.clone()), s)
    })?;

    slog::info!(
        log,
        "{:?} {:?} vcr replace requested",
        disk_name,
        path_params.id,
    );

    // Try the replacement.
    // Crucible does the heavy lifting here to verify that the old/new
    // VCRs are different in just the correct way and will return error
    // if there is any mismatch.
    backend.vcr_replace(old_vcr_json, &new_vcr_json).await.map_err(|e| {
        HttpError::for_bad_request(Some(e.to_string()), e.to_string())
    })?;

    // Our replacement request was accepted.  We now need to update the
    // spec stored in propolis so it matches what the downstairs now has.
    let new_storage_backend: StorageBackendV0 =
        StorageBackendV0::Crucible(CrucibleStorageBackend {
            readonly,
            request_json: new_vcr_json,
        });
    v0_spec.backends.storage_backends.insert(disk_name, new_storage_backend);

    slog::info!(log, "Replaced the VCR in backend of {:?}", path_params.id);

    Ok(HttpResponseOk(()))
}

/// Issues an NMI to the instance.
#[endpoint {
    method = POST,
    path = "/instance/nmi",
}]
async fn instance_issue_nmi(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<()>, HttpError> {
    let vm = rqctx.context().vm().await?;
    vm.inject_nmi();

    Ok(HttpResponseOk(()))
}

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<Arc<DropshotEndpointContext>> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_spec_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_spec_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();
    api.register(instance_serial_history_get).unwrap();
    api.register(instance_migrate_start).unwrap();
    api.register(instance_migrate_status).unwrap();
    api.register(instance_issue_crucible_snapshot_request).unwrap();
    api.register(instance_issue_crucible_vcr_request).unwrap();
    api.register(instance_issue_nmi).unwrap();

    api
}

fn not_created_error() -> HttpError {
    HttpError::for_client_error(
        Some(api::ErrorCode::NoInstance.to_string()),
        http::StatusCode::FAILED_DEPENDENCY,
        "Server not initialized (no instance)".to_string(),
    )
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
