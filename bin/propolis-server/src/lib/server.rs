// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP server callback functions.
//!
//! Functions in this module verify parameters and convert between types (API
//! request types to Propolis-native types and Propolis-native error types to
//! HTTP error codes) before sending operations to the VM state machine for
//! processing.

use std::convert::TryFrom;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use std::sync::Arc;

use crate::{
    serial::history_buffer::SerialHistoryOffset,
    spec::{
        self,
        api_spec_v0::ApiSpecError,
        builder::{SpecBuilder, SpecBuilderError},
        Spec,
    },
    vm::{ensure::VmEnsureRequest, VmError},
    vnc::{self, VncServer},
};

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
    self, components::devices::QemuPvpanic, VersionedInstanceSpec,
};
pub use propolis_server_config::Config as VmTomlConfig;
use rfb::tungstenite::BinaryWs;
use slog::{error, warn, Logger};
use tokio::sync::MutexGuard;
use tokio_tungstenite::{
    tungstenite::protocol::{Role, WebSocketConfig},
    WebSocketStream,
};

/// Configuration used to set this server up to provide Oximeter metrics.
#[derive(Debug, Clone)]
pub struct MetricsEndpointConfig {
    /// The address at which the Oximeter endpoint will be hosted (i.e., this
    /// server's address).
    pub listen_addr: IpAddr,

    /// The address of the Nexus instance with which we should register our own
    /// server's address.
    ///
    /// If this is None _and the listen address is IPv6_, then internal DNS will
    /// be used to register for metrics.
    pub registration_addr: Option<SocketAddr>,
}

/// Static configuration for objects owned by this server. The server obtains
/// this configuration at startup time and refers to it when manipulating its
/// objects.
pub struct StaticConfig {
    /// The TOML-driven configuration for this server's instances.
    pub vm: Arc<VmTomlConfig>,

    /// Whether to use the host's guest memory reservoir to back guest memory.
    pub use_reservoir: bool,

    /// The configuration to use when setting up this server's Oximeter
    /// endpoint.
    metrics: Option<MetricsEndpointConfig>,
}

/// Context accessible from HTTP callbacks.
pub struct DropshotEndpointContext {
    static_config: StaticConfig,
    pub vnc_server: Arc<VncServer>,
    pub(crate) vm: Arc<crate::vm::Vm>,
    log: Logger,
}

impl DropshotEndpointContext {
    /// Creates a new server context object.
    pub fn new(
        config: VmTomlConfig,
        use_reservoir: bool,
        log: slog::Logger,
        metric_config: Option<MetricsEndpointConfig>,
    ) -> Self {
        let vnc_server = VncServer::new(log.clone());
        Self {
            static_config: StaticConfig {
                vm: Arc::new(config),
                use_reservoir,
                metrics: metric_config,
            },
            vnc_server,
            vm: crate::vm::Vm::new(&log),
            log,
        }
    }
}

/// Creates an instance spec from an ensure request. (Both types are foreign to
/// this crate, so implementing TryFrom for them is not allowed.)
fn instance_spec_from_request(
    request: &api::InstanceEnsureRequest,
    toml_config: &VmTomlConfig,
) -> Result<Spec, SpecBuilderError> {
    let mut spec_builder = SpecBuilder::new(&request.properties);

    spec_builder.add_devices_from_config(toml_config)?;

    for nic in &request.nics {
        spec_builder.add_nic_from_request(nic)?;
    }

    for disk in &request.disks {
        spec_builder.add_disk_from_request(disk)?;
    }

    if let Some(boot_settings) = request.boot_settings.as_ref() {
        for item in boot_settings.order.iter() {
            spec_builder.add_boot_option(item)?;
        }
    }

    if let Some(base64) = &request.cloud_init_bytes {
        spec_builder.add_cloud_init_from_request(base64.clone())?;
    }

    for (name, port) in [
        ("com1", instance_spec::components::devices::SerialPortNumber::Com1),
        ("com2", instance_spec::components::devices::SerialPortNumber::Com2),
        ("com3", instance_spec::components::devices::SerialPortNumber::Com3),
        // SoftNpu uses this port for ASIC management.
        #[cfg(not(feature = "falcon"))]
        ("com4", instance_spec::components::devices::SerialPortNumber::Com4),
    ] {
        spec_builder.add_serial_port(name.to_owned(), port)?;
    }

    #[cfg(feature = "falcon")]
    spec_builder.set_softnpu_com4("com4".to_owned())?;

    spec_builder.add_pvpanic_device(spec::QemuPvpanic {
        name: "pvpanic".to_string(),
        spec: QemuPvpanic { enable_isa: true },
    })?;

    Ok(spec_builder.finish())
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
                warn!(log, "Failed to resolve Nexus endpoint"; "error" => ?e);
                None
            }
        },
        Err(e) => {
            warn!(log, "Failed to create Nexus client"; "error" => ?e);
            None
        }
    }
}

async fn instance_ensure_common(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    properties: api::InstanceProperties,
    migrate: Option<api::InstanceMigrateInitiateRequest>,
    instance_spec: Spec,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    let server_context = rqctx.context();
    let oximeter_registry = server_context
        .static_config
        .metrics
        .as_ref()
        .map(|_| ProducerRegistry::with_id(properties.id));

    let nexus_client =
        find_local_nexus_client(rqctx.server.local_addr, rqctx.log.clone())
            .await;

    let ensure_options = crate::vm::EnsureOptions {
        toml_config: server_context.static_config.vm.clone(),
        use_reservoir: server_context.static_config.use_reservoir,
        metrics_config: server_context.static_config.metrics.clone(),
        oximeter_registry,
        nexus_client,
        vnc_server: server_context.vnc_server.clone(),
        local_server_addr: rqctx.server.local_addr,
    };

    let request = VmEnsureRequest { properties, migrate, instance_spec };
    server_context
        .vm
        .ensure(&server_context.log, request, ensure_options)
        .await
        .map(HttpResponseCreated)
        .map_err(|e| match e {
            VmError::ResultChannelClosed => HttpError::for_internal_error(
                "state driver unexpectedly dropped result channel".to_string(),
            ),
            VmError::WaitingToInitialize
            | VmError::AlreadyInitialized
            | VmError::RundownInProgress => HttpError::for_client_error(
                Some(api::ErrorCode::AlreadyInitialized.to_string()),
                http::StatusCode::CONFLICT,
                "instance already initialized".to_string(),
            ),
            VmError::InitializationFailed(e) => HttpError::for_internal_error(
                format!("VM initialization failed: {e}"),
            ),
            _ => HttpError::for_internal_error(format!(
                "unexpected error from VM controller: {e}"
            )),
        })
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
                        "failed to generate instance spec from request: {:#?}",
                        e
                    ),
                )
            })?;

    instance_ensure_common(
        rqctx,
        request.properties,
        request.migrate,
        instance_spec,
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
    let request = request.into_inner();
    let VersionedInstanceSpec::V0(v0_spec) = request.instance_spec;
    let spec = Spec::try_from(v0_spec).map_err(|e: ApiSpecError| {
        HttpError::for_bad_request(None, e.to_string())
    })?;

    instance_ensure_common(rqctx, request.properties, request.migrate, spec)
        .await
}

async fn instance_get_common(
    rqctx: &RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<api::InstanceSpecGetResponse, HttpError> {
    let ctx = rqctx.context();
    ctx.vm.get().await.map_err(|e| match e {
        VmError::NotCreated | VmError::WaitingToInitialize => {
            not_created_error()
        }
        _ => HttpError::for_internal_error(format!(
            "unexpected error from VM controller: {e}"
        )),
    })
}

#[endpoint {
    method = GET,
    path = "/instance/spec",
}]
async fn instance_spec_get(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<api::InstanceSpecGetResponse>, HttpError> {
    Ok(HttpResponseOk(instance_get_common(&rqctx).await?))
}

#[endpoint {
    method = GET,
    path = "/instance",
}]
async fn instance_get(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    instance_get_common(&rqctx).await.map(|full| {
        HttpResponseOk(api::InstanceGetResponse {
            instance: api::Instance {
                properties: full.properties,
                state: full.state,
                disks: vec![],
                nics: vec![],
            },
        })
    })
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
    let mut state_watcher =
        ctx.vm.state_watcher().await.map_err(|e| match e {
            VmError::NotCreated | VmError::WaitingToInitialize => {
                not_created_error()
            }
            _ => HttpError::for_internal_error(format!(
                "unexpected error from VM controller: {e}"
            )),
        })?;

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
    let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
    let result = vm
        .put_state(requested_state)
        .map(|_| HttpResponseUpdatedNoContent {})
        .map_err(|e| match e {
            VmError::NotCreated | VmError::WaitingToInitialize => {
                not_created_error()
            }
            VmError::ForbiddenStateChange(reason) => HttpError::for_status(
                Some(format!("instance state change not allowed: {}", reason)),
                http::status::StatusCode::FORBIDDEN,
            ),
            _ => HttpError::for_internal_error(format!(
                "unexpected error from VM controller: {e}"
            )),
        });

    if result.is_ok() {
        if let api::InstanceStateRequested::Reboot = requested_state {
            let stats =
                MutexGuard::map(vm.services().oximeter.lock().await, |state| {
                    &mut state.stats
                });
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
    let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
    let serial = vm.objects().lock_shared().await.com1().clone();
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
    let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
    let serial = vm.objects().lock_shared().await.com1().clone();

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
    let serial_task = vm.services().serial_task.lock().await;
    serial_task
        .as_ref()
        .ok_or("Instance has no serial task")?
        .websocks_ch
        .send(ws_stream)
        .await
        .map_err(|e| format!("Serial socket hand-off failed: {}", e).into())
}

#[channel {
    protocol = WEBSOCKETS,
    path = "/instance/vnc",
    unpublished = true,
}]
async fn instance_vnc(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    _query: Query<()>,
    websock: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let ctx = rqctx.context();

    let ws_stream = WebSocketStream::from_raw_socket(
        websock.into_inner(),
        Role::Server,
        None,
    )
    .await;

    if let Err(e) = ctx
        .vnc_server
        .connect(
            Box::new(BinaryWs::new(ws_stream)) as Box<dyn vnc::Connection>,
            rqctx.request_id.clone(),
        )
        .await
    {
        // Log the error, but since the request has already been upgraded, there
        // is no sense in trying to emit a formal error to the client
        error!(rqctx.log, "VNC initialization failed: {:?}", e);
    }

    Ok(())
}

// This endpoint is meant to only be called during a migration from the
// destination instance to the source instance as part of the HTTP connection
// upgrade used to establish the migration link. We don't actually want this
// exported via OpenAPI clients.
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
    let ctx = rqctx.context();
    let migration_id = path_params.into_inner().migration_id;
    let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
    Ok(vm.request_migration_out(migration_id, websock).await?)
}

#[endpoint {
    method = GET,
    path = "/instance/migration-status"
}]
async fn instance_migrate_status(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<api::InstanceMigrateStatusResponse>, HttpError> {
    let ctx = rqctx.context();
    ctx.vm
        .state_watcher()
        .await
        .map(|rx| HttpResponseOk(rx.borrow().migration.clone()))
        .map_err(|e| match e {
            VmError::NotCreated | VmError::WaitingToInitialize => {
                not_created_error()
            }
            _ => HttpError::for_internal_error(format!(
                "unexpected error from VM controller: {e}"
            )),
        })
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
    let vm =
        rqctx.context().vm.active_vm().await.ok_or_else(not_created_error)?;
    let objects = vm.objects().lock_shared().await;
    let path_params = path_params.into_inner();

    let backend =
        objects.crucible_backends().get(&path_params.id).ok_or_else(|| {
            let s = format!("no disk with id {}!", path_params.id);
            HttpError::for_not_found(Some(s.clone()), s)
        })?;
    backend.snapshot(path_params.snapshot_id).await.map_err(|e| {
        HttpError::for_bad_request(Some(e.to_string()), e.to_string())
    })?;

    Ok(HttpResponseOk(()))
}

/// Gets the status of a Crucible volume backing a disk
#[endpoint {
    method = GET,
    path = "/instance/disk/{id}/status",
}]
async fn disk_volume_status(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
    path_params: Path<api::VolumeStatusPathParams>,
) -> Result<HttpResponseOk<api::VolumeStatus>, HttpError> {
    let path_params = path_params.into_inner();
    let vm =
        rqctx.context().vm.active_vm().await.ok_or_else(not_created_error)?;
    let objects = vm.objects().lock_shared().await;
    let backend =
        objects.crucible_backends().get(&path_params.id).ok_or_else(|| {
            let s = format!("No crucible backend for id {}", path_params.id);
            HttpError::for_not_found(Some(s.clone()), s)
        })?;

    Ok(HttpResponseOk(api::VolumeStatus {
        active: backend.volume_is_active().await.map_err(|e| {
            HttpError::for_bad_request(Some(e.to_string()), e.to_string())
        })?,
    }))
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
) -> Result<HttpResponseOk<crucible_client_types::ReplaceResult>, HttpError> {
    let path_params = path_params.into_inner();
    let request = request.into_inner();
    let new_vcr_json = request.vcr_json;
    let disk_name = request.name;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let vm =
        rqctx.context().vm.active_vm().await.ok_or_else(not_created_error)?;

    vm.reconfigure_crucible_volume(disk_name, path_params.id, new_vcr_json, tx)
        .map_err(|e| match e {
            VmError::ForbiddenStateChange(reason) => HttpError::for_status(
                Some(format!("instance state change not allowed: {}", reason)),
                http::status::StatusCode::FORBIDDEN,
            ),
            _ => HttpError::for_internal_error(format!(
                "unexpected error from VM controller: {e}"
            )),
        })?;

    let result = rx.await.map_err(|_| {
        HttpError::for_internal_error(
            "VM worker task unexpectedly dropped result channel".to_string(),
        )
    })?;

    result.map(HttpResponseOk)
}

/// Issues an NMI to the instance.
#[endpoint {
    method = POST,
    path = "/instance/nmi",
}]
async fn instance_issue_nmi(
    rqctx: RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<HttpResponseOk<()>, HttpError> {
    let vm =
        rqctx.context().vm.active_vm().await.ok_or_else(not_created_error)?;
    let _ = vm.objects().lock_shared().await.machine().inject_nmi();

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
    api.register(disk_volume_status).unwrap();
    api.register(instance_issue_crucible_vcr_request).unwrap();
    api.register(instance_issue_nmi).unwrap();
    api.register(instance_vnc).unwrap();

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
mod test {
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
