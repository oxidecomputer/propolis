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
use std::error::Error;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use std::path::PathBuf;
use std::sync::Arc;

use crate::migrate::destination::MigrationTargetInfo;
use crate::vm::ensure::VmInitializationMethod;
use crate::{
    serial::history_buffer::SerialHistoryOffset,
    vm::{ensure::VmEnsureRequest, VmError},
    vnc::{self, VncServer},
};

use dropshot::{
    ApiDescription, ClientErrorStatusCode, HttpError, HttpResponseCreated,
    HttpResponseOk, HttpResponseUpdatedNoContent, Path, Query, RequestContext,
    TypedBody, WebsocketConnection,
};
use futures::SinkExt;
use internal_dns_resolver::{ResolveError, Resolver};
use internal_dns_types::names::ServiceName;
pub use nexus_client::Client as NexusClient;
use oximeter::types::ProducerRegistry;
use propolis_api_types as api;
use propolis_api_types::instance_spec::{
    v0::InstanceSpecGetResponseV0, SpecKey,
};
use propolis_api_types::v0::InstanceInitializationMethodV0;
use propolis_api_types::InstanceInitializationMethod;
use propolis_server_api::PropolisServerApi;
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
    /// The path to the bootrom image to expose to the guest.
    pub bootrom_path: PathBuf,

    /// The bootrom version string to expose to the guest. If None, machine
    /// initialization chooses a default value.
    pub bootrom_version: Option<String>,

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
        bootrom_path: PathBuf,
        bootrom_version: Option<String>,
        use_reservoir: bool,
        log: slog::Logger,
        metric_config: Option<MetricsEndpointConfig>,
    ) -> Self {
        let vnc_server = VncServer::new(log.clone());
        Self {
            static_config: StaticConfig {
                bootrom_path,
                bootrom_version,
                use_reservoir,
                metrics: metric_config,
            },
            vnc_server,
            vm: crate::vm::Vm::new(&log),
            log,
        }
    }
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
            &format!("http://{address}"),
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

// DEPRECATED
async fn v0_instance_get(
    rqctx: &RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<InstanceSpecGetResponseV0, HttpError> {
    let ctx = rqctx.context();
    ctx.vm.v0_get().await.ok_or_else(not_created_error)
}

async fn instance_get(
    rqctx: &RequestContext<Arc<DropshotEndpointContext>>,
) -> Result<api::InstanceSpecGetResponse, HttpError> {
    let ctx = rqctx.context();
    ctx.vm.get().await.ok_or_else(not_created_error)
}

enum PropolisServerImpl {}

impl PropolisServerApi for PropolisServerImpl {
    type Context = Arc<DropshotEndpointContext>;

    async fn instance_ensure(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<api::InstanceEnsureRequest>,
    ) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError>
    {
        let server_context = rqctx.context();
        let api::InstanceEnsureRequest { properties, init } =
            request.into_inner();
        let oximeter_registry = server_context
            .static_config
            .metrics
            .as_ref()
            .map(|_| ProducerRegistry::with_id(properties.id));

        let nexus_client =
            find_local_nexus_client(rqctx.server.local_addr, rqctx.log.clone())
                .await;

        let ensure_options = crate::vm::EnsureOptions {
            bootrom_path: server_context.static_config.bootrom_path.clone(),
            bootrom_version: server_context
                .static_config
                .bootrom_version
                .clone(),
            use_reservoir: server_context.static_config.use_reservoir,
            metrics_config: server_context.static_config.metrics.clone(),
            oximeter_registry,
            nexus_client,
            vnc_server: server_context.vnc_server.clone(),
            local_server_addr: rqctx.server.local_addr,
        };

        let vm_init = match init {
            InstanceInitializationMethod::Spec { spec } => spec
                .try_into()
                .map(|s| VmInitializationMethod::Spec(Box::new(s)))
                .map_err(|e| {
                    if let Some(s) = e.source() {
                        format!("{e}: {s}")
                    } else {
                        e.to_string()
                    }
                }),
            InstanceInitializationMethod::MigrationTarget {
                migration_id,
                src_addr,
                replace_components,
            } => Ok(VmInitializationMethod::Migration(MigrationTargetInfo {
                migration_id,
                src_addr,
                replace_components,
            })),
        }
        .map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to generate internal instance spec: {e}"),
            )
        })?;

        let request = VmEnsureRequest { properties, init: vm_init };
        server_context
            .vm
            .ensure(&server_context.log, request, ensure_options)
            .await
            .map(HttpResponseCreated)
            .map_err(|e| match e {
                VmError::ResultChannelClosed => HttpError::for_internal_error(
                    "state driver unexpectedly dropped result channel"
                        .to_string(),
                ),
                VmError::WaitingToInitialize
                | VmError::AlreadyInitialized
                | VmError::RundownInProgress => HttpError::for_client_error(
                    Some(api::ErrorCode::AlreadyInitialized.to_string()),
                    ClientErrorStatusCode::CONFLICT,
                    "instance already initialized".to_string(),
                ),
                VmError::InitializationFailed(e) => {
                    HttpError::for_internal_error(format!(
                        "VM initialization failed: {e}"
                    ))
                }
                _ => HttpError::for_internal_error(format!(
                    "unexpected error from VM controller: {e}"
                )),
            })
    }

    async fn v0_instance_ensure(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<api::v0::InstanceEnsureRequestV0>,
    ) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError>
    {
        let server_context = rqctx.context();
        let api::v0::InstanceEnsureRequestV0 { properties, init } =
            request.into_inner();
        let oximeter_registry = server_context
            .static_config
            .metrics
            .as_ref()
            .map(|_| ProducerRegistry::with_id(properties.id));

        let nexus_client =
            find_local_nexus_client(rqctx.server.local_addr, rqctx.log.clone())
                .await;

        let ensure_options = crate::vm::EnsureOptions {
            bootrom_path: server_context.static_config.bootrom_path.clone(),
            bootrom_version: server_context
                .static_config
                .bootrom_version
                .clone(),
            use_reservoir: server_context.static_config.use_reservoir,
            metrics_config: server_context.static_config.metrics.clone(),
            oximeter_registry,
            nexus_client,
            vnc_server: server_context.vnc_server.clone(),
            local_server_addr: rqctx.server.local_addr,
        };

        let vm_init = match init {
            InstanceInitializationMethodV0::Spec { spec } => spec
                .try_into()
                .map(|s| VmInitializationMethod::Spec(Box::new(s)))
                .map_err(|e| {
                    if let Some(s) = e.source() {
                        format!("{e}: {s}")
                    } else {
                        e.to_string()
                    }
                }),
            InstanceInitializationMethodV0::MigrationTarget {
                migration_id,
                src_addr,
                replace_components,
            } => Ok(VmInitializationMethod::Migration(MigrationTargetInfo {
                migration_id,
                src_addr,
                replace_components,
            })),
        }
        .map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to generate internal instance spec: {e}"),
            )
        })?;

        let request = VmEnsureRequest { properties, init: vm_init };
        server_context
            .vm
            .ensure(&server_context.log, request, ensure_options)
            .await
            .map(HttpResponseCreated)
            .map_err(|e| match e {
                VmError::ResultChannelClosed => HttpError::for_internal_error(
                    "state driver unexpectedly dropped result channel"
                        .to_string(),
                ),
                VmError::WaitingToInitialize
                | VmError::AlreadyInitialized
                | VmError::RundownInProgress => HttpError::for_client_error(
                    Some(api::ErrorCode::AlreadyInitialized.to_string()),
                    ClientErrorStatusCode::CONFLICT,
                    "instance already initialized".to_string(),
                ),
                VmError::InitializationFailed(e) => {
                    HttpError::for_internal_error(format!(
                        "VM initialization failed: {e}"
                    ))
                }
                _ => HttpError::for_internal_error(format!(
                    "unexpected error from VM controller: {e}"
                )),
            })
    }

    async fn instance_spec_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<api::InstanceSpecGetResponse>, HttpError> {
        Ok(HttpResponseOk(instance_get(&rqctx).await?))
    }

    async fn v0_instance_spec_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<InstanceSpecGetResponseV0>, HttpError> {
        Ok(HttpResponseOk(v0_instance_get(&rqctx).await?))
    }

    async fn instance_get(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
        instance_get(&rqctx).await.map(|full| {
            HttpResponseOk(api::InstanceGetResponse {
                instance: api::Instance {
                    properties: full.properties,
                    state: full.state,
                },
            })
        })
    }

    async fn instance_state_monitor(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<api::InstanceStateMonitorRequest>,
    ) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError>
    {
        let ctx = rqctx.context();
        let gen = request.into_inner().gen;
        let mut state_watcher =
            ctx.vm.state_watcher().await.ok_or_else(not_created_error)?;

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
                    ClientErrorStatusCode::GONE,
                    format!(
                        "No instance present; will never reach generation {gen}",
                    ),
                )
            })?;
        }
    }

    async fn instance_state_put(
        rqctx: RequestContext<Self::Context>,
        request: TypedBody<api::InstanceStateRequested>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError> {
        let ctx = rqctx.context();
        let requested_state = request.into_inner();
        let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
        let result = vm
            .put_state(requested_state)
            .map(|_| HttpResponseUpdatedNoContent {})
            .map_err(|e| match e {
                VmError::WaitingToInitialize => HttpError::for_unavail(
                    None,
                    "instance is still initializing".to_string(),
                ),
                VmError::ForbiddenStateChange(reason) => {
                    HttpError::for_client_error_with_status(
                        Some(format!(
                            "instance state change not allowed: {reason}"
                        )),
                        ClientErrorStatusCode::FORBIDDEN,
                    )
                }
                _ => HttpError::for_internal_error(format!(
                    "unexpected error from VM controller: {e}"
                )),
            });

        if result.is_ok() {
            if let api::InstanceStateRequested::Reboot = requested_state {
                let stats = MutexGuard::map(
                    vm.services().oximeter.lock().await,
                    |state| &mut state.stats,
                );
                if let Some(stats) = stats.as_ref() {
                    stats.count_reset();
                }
            }
        }

        result
    }

    async fn instance_shutdown(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError> {
        let ctx = rqctx.context();
        let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
        let power_button = vm.objects().lock_shared().await.power_button().clone();
        power_button.pulse();

        Ok(HttpResponseUpdatedNoContent {})
    }

    async fn instance_serial_history_get(
        rqctx: RequestContext<Self::Context>,
        query: Query<api::InstanceSerialConsoleHistoryRequest>,
    ) -> Result<
        HttpResponseOk<api::InstanceSerialConsoleHistoryResponse>,
        HttpError,
    > {
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

    async fn instance_serial(
        rqctx: RequestContext<Self::Context>,
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

        let byte_offset =
            SerialHistoryOffset::try_from(&query.into_inner()).ok();
        if let Some(mut byte_offset) = byte_offset {
            loop {
                let (data, offset) =
                    serial.history_vec(byte_offset, None).await?;
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
            .map_err(|e| format!("Serial socket hand-off failed: {e}").into())
    }

    async fn instance_vnc(
        rqctx: RequestContext<Self::Context>,
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

    async fn instance_migrate_start(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<api::InstanceMigrateStartRequest>,
        websock: WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult {
        let ctx = rqctx.context();
        let migration_id = path_params.into_inner().migration_id;
        let vm = ctx.vm.active_vm().await.ok_or_else(not_created_error)?;
        Ok(vm.request_migration_out(migration_id, websock).await?)
    }

    async fn instance_migrate_status(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<api::InstanceMigrateStatusResponse>, HttpError>
    {
        let ctx = rqctx.context();
        ctx.vm
            .state_watcher()
            .await
            .map(|rx| HttpResponseOk(rx.borrow().migration.clone()))
            .ok_or_else(not_created_error)
    }

    async fn instance_issue_crucible_snapshot_request(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<api::SnapshotRequestPathParams>,
    ) -> Result<HttpResponseOk<()>, HttpError> {
        let vm = rqctx
            .context()
            .vm
            .active_vm()
            .await
            .ok_or_else(not_created_error)?;
        let objects = vm.objects().lock_shared().await;
        let path_params = path_params.into_inner();

        let backend = objects
            .crucible_backends()
            .get(&SpecKey::from(path_params.id.clone()))
            .ok_or_else(|| {
                let s = format!("no disk with id {}!", path_params.id);
                HttpError::for_not_found(Some(s.clone()), s)
            })?;
        backend.snapshot(path_params.snapshot_id).await.map_err(|e| {
            HttpError::for_bad_request(Some(e.to_string()), e.to_string())
        })?;

        Ok(HttpResponseOk(()))
    }

    async fn disk_volume_status(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<api::VolumeStatusPathParams>,
    ) -> Result<HttpResponseOk<api::VolumeStatus>, HttpError> {
        let path_params = path_params.into_inner();
        let vm = rqctx
            .context()
            .vm
            .active_vm()
            .await
            .ok_or_else(not_created_error)?;
        let objects = vm.objects().lock_shared().await;
        let backend = objects
            .crucible_backends()
            .get(&SpecKey::from(path_params.id.clone()))
            .ok_or_else(|| {
                let s =
                    format!("No crucible backend for id {}", path_params.id);
                HttpError::for_not_found(Some(s.clone()), s)
            })?;

        Ok(HttpResponseOk(api::VolumeStatus {
            active: backend.volume_is_active().await.map_err(|e| {
                HttpError::for_bad_request(Some(e.to_string()), e.to_string())
            })?,
        }))
    }

    async fn instance_issue_crucible_vcr_request(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<api::VCRRequestPathParams>,
        request: TypedBody<api::InstanceVCRReplace>,
    ) -> Result<HttpResponseOk<crucible_client_types::ReplaceResult>, HttpError>
    {
        let path_params = path_params.into_inner();
        let request = request.into_inner();
        let new_vcr_json = request.vcr_json;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let vm = rqctx
            .context()
            .vm
            .active_vm()
            .await
            .ok_or_else(not_created_error)?;

        vm.reconfigure_crucible_volume(
            SpecKey::from(path_params.id),
            new_vcr_json,
            tx,
        )
        .map_err(|e| match e {
            VmError::ForbiddenStateChange(reason) => {
                HttpError::for_client_error_with_status(
                    Some(format!(
                        "instance state change not allowed: {reason}"
                    )),
                    ClientErrorStatusCode::FORBIDDEN,
                )
            }
            _ => HttpError::for_internal_error(format!(
                "unexpected error from VM controller: {e}"
            )),
        })?;

        let result = rx.await.map_err(|_| {
            HttpError::for_internal_error(
                "VM worker task unexpectedly dropped result channel"
                    .to_string(),
            )
        })?;

        result.map(HttpResponseOk)
    }

    async fn instance_issue_nmi(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<()>, HttpError> {
        let vm = rqctx
            .context()
            .vm
            .active_vm()
            .await
            .ok_or_else(not_created_error)?;
        let _ = vm.objects().lock_shared().await.machine().inject_nmi();

        Ok(HttpResponseOk(()))
    }
}

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<Arc<DropshotEndpointContext>> {
    propolis_server_api::propolis_server_api_mod::api_description::<
        PropolisServerImpl,
    >()
    .unwrap()
}

fn not_created_error() -> HttpError {
    HttpError::for_client_error(
        Some(api::ErrorCode::NoInstance.to_string()),
        ClientErrorStatusCode::FAILED_DEPENDENCY,
        "Server not initialized (no instance)".to_string(),
    )
}
