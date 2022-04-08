//! Implementation of a mock Propolis server

use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::HttpError;
use dropshot::HttpResponseCreated;
use dropshot::HttpResponseOk;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::Path;
use dropshot::RequestContext;
use dropshot::TypedBody;
use futures::future::Fuse;
use futures::{FutureExt, SinkExt, StreamExt};
use hyper::upgrade::{self, Upgraded};
use hyper::{header, Body, Response, StatusCode};
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
use propolis::hw::pci;
use propolis::hw::uart::LpcUart;
use propolis::instance::Instance;
use propolis_client::handmade::api;

use crate::config::Config;
use crate::initializer::{build_instance, MachineInitializer};
use crate::migrate;
use crate::serial::Serial;

/// ed instance properties
pub struct InstanceContext {
    pub state: propolis::instance::State,
    pub generation: u64,
    pub properties: api::InstanceProperties,
    state_watcher_rx: watch::Receiver<StateChange>,
    state_watcher_tx: watch::Sender<StateChange>,
}

impl InstanceContext {
    pub fn new(properties: api::InstanceProperties) -> Self {
        let (state_watcher_tx, state_watcher_rx) =
            watch::channel(StateChange {
                gen: 0,
                state: propolis::instance::State::Initialize,
            });
        Self {
            state: propolis::instance::State::Initialize,
            generation: 0,
            properties,
            state_watcher_rx,
            state_watcher_tx,
        }
    }

    /// Updates the state of the mock instance.
    ///
    /// Returns an error if the state transition is invalid.
    pub fn set_target_state(
        &mut self,
        target: propolis::instance::ReqState,
    ) -> Result<(), propolis::instance::TransitionError> {
        use propolis::instance::ReqState;
        use propolis::instance::State;
        use propolis::instance::TransitionError;

        if matches!(self.state, State::Halt | State::Destroy) {
            // Cannot request any state once the target is halt/destroy
            return Err(TransitionError::Terminal);
        }
        if self.state == State::Reset && target == ReqState::Run {
            // Requesting a run when already on the road to reboot is an
            // immediate success.
            return Ok(());
        }
        match target {
            ReqState::Run | ReqState::Reset => {
                self.generation += 1;
                self.state = State::Run;
                let result = self.state_watcher_tx.send(StateChange {
                    gen: self.generation,
                    state: self.state,
                });
                assert!(
                    result.is_ok(),
                    "Failed to send simulated state change update"
                );
            }
            ReqState::Halt => self.state = State::Halt,
            ReqState::StartMigrate => {
                unimplemented!("migration not yet implemented")
            }
        }
        Ok(())
    }
}

/// Contextual information accessible from mock HTTP callbacks.
pub struct Context {
    instance: Mutex<Option<InstanceContext>>,
    _config: Config,
    log: Logger,
}

impl Context {
    pub fn new(config: Config, log: Logger) -> Self {
        Context { instance: Mutex::new(None), _config: config, log }
    }
}

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

    // Handle an already-initialized instance
    let mut instance = server_context.instance.lock().await;
    if let Some(instance) = &*instance {
        if instance.properties.id != instance_id {
            return Err(HttpError::for_internal_error(format!(
                "Server already initialized with ID {}",
                instance.properties.id
            )));
        }
        if instance.properties != properties {
            return Err(HttpError::for_internal_error(
                "Cannot update running server".to_string(),
            ));
        }
        return Ok(HttpResponseCreated(api::InstanceEnsureResponse {
            migrate: None,
        }));
    }

    // Perform some basic validation of the requested properties
    for nic in &nics {
        info!(server_context.log, "Creating NIC: {:#?}", nic);
        slot_to_bdf(nic.slot, SlotType::NIC).map_err(|e| {
            let err = Error::new(
                ErrorKind::InvalidData,
                format!("Cannot parse vnic PCI: {}", e),
            );
            HttpError::for_internal_error(format!(
                "Cannot build instance: {}",
                err
            ))
        })?;
    }

    for disk in &disks {
        info!(server_context.log, "Creating Disk: {:#?}", disk);
        slot_to_bdf(disk.slot, SlotType::Disk).map_err(|e| {
            let err = Error::new(
                ErrorKind::InvalidData,
                format!("Cannot parse disk PCI: {}", e),
            );
            HttpError::for_internal_error(format!(
                "Cannot build instance: {}",
                err
            ))
        })?;
        info!(server_context.log, "Disk {} created successfully", disk.name);
    }

    if let Some(cloud_init_bytes) = &cloud_init_bytes {
        info!(server_context.log, "Creating cloud-init disk");
        slot_to_bdf(api::Slot(0), SlotType::CloudInit).map_err(|e| {
            let err = Error::new(ErrorKind::InvalidData, e.to_string());
            HttpError::for_internal_error(format!(
                "Cannot build instance: {}",
                err
            ))
        })?;
        base64::decode(&cloud_init_bytes).map_err(|e| {
            let err = Error::new(ErrorKind::InvalidInput, e.to_string());
            HttpError::for_internal_error(format!(
                "Cannot build instance: {}",
                err
            ))
        })?;
        info!(server_context.log, "cloud-init disk created");
    }

    *instance = Some(InstanceContext::new(properties));
    Ok(HttpResponseCreated(api::InstanceEnsureResponse { migrate: None }))
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
    let instance = rqctx.context().instance.lock().await;
    let instance = instance.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;
    if path_params.into_inner().instance_id != instance.properties.name {
        return Err(HttpError::for_internal_error(
            "Instance name mismatch (path did not match struct)".to_string(),
        ));
    }
    Ok(HttpResponseOk(instance.properties.id))
}

#[endpoint {
    method = GET,
    path = "/instances/{instance_id}",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<Context>>,
    path_params: Path<api::InstancePathParams>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    let instance = rqctx.context().instance.lock().await;
    let instance = instance.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;
    if path_params.into_inner().instance_id != instance.properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }
    let instance_info = api::Instance {
        properties: instance.properties.clone(),
        state: propolis_to_api_state(instance.state),
        disks: vec![],
        nics: vec![],
    };
    Ok(HttpResponseOk(api::InstanceGetResponse { instance: instance_info }))
}

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
        let instance = rqctx.context().instance.lock().await;
        let instance = instance.as_ref().ok_or_else(|| {
            HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            )
        })?;
        let path_params = path_params.into_inner();
        if path_params.instance_id != instance.properties.id {
            return Err(HttpError::for_internal_error(
                "UUID mismatch (path did not match struct)".to_string(),
            ));
        }
        let gen = request.into_inner().gen;
        let state_watcher = instance.state_watcher_rx.clone();
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
    let mut instance = rqctx.context().instance.lock().await;
    let instance = instance.as_mut().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;
    if path_params.into_inner().instance_id != instance.properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }
    let requested_state = api_to_propolis_state(request.into_inner());
    instance.set_target_state(requested_state).map_err(|err| {
        HttpError::for_internal_error(format!("Failed to transition: {}", err))
    })?;
    Ok(HttpResponseUpdatedNoContent {})
}

/// Returns a Dropshot [`ApiDescription`] object to launch a mock Propolis
/// server.
pub fn api() -> ApiDescription<Context> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get_uuid).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api
}
