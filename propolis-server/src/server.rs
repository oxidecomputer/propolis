use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, HttpServerStarter, Path, RequestContext,
    TypedBody,
};
use futures::FutureExt;
use std::sync::Arc;
use propolis_client::api;
use tokio::sync::Mutex;
use crate::config;

// TODO(error) Do a pass of HTTP codes (error and ok)
// TODO(idempotency) Idempotency mechanisms?

pub trait InstanceCtx: Sized + Send + Sync + 'static {
    fn new(properties: api::InstanceProperties, config: &config::Config) -> Result<Self, HttpError>;
}

pub struct ServerContext<I: InstanceCtx> {
    context: Mutex<Option<I>>,
    config: config::Config,
}

impl<I: InstanceCtx> ServerContext<I> {
    pub fn new(config: config::Config) -> Self {
        ServerContext { context: Mutex::new(None), config }
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
        PropolisState::Quiesce => ApiState::Stopped,
        PropolisState::Halt => todo!(),
        PropolisState::Reset => todo!(),
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
pub async fn instance_ensure(
    rqctx: Arc<RequestContext<ServerContext<impl InstanceCtx>>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceEnsureRequest>,
) -> Result<HttpResponseCreated<api::InstanceEnsureResponse>, HttpError> {
    let server_context = rqctx.context();

    // TODO(idempotency) - how?

    let mut context = server_context.context.lock().await;

    if context.is_some() {
        return Err(HttpError::for_internal_error(
            "Server already initialized".to_string(),
        ));
    }

    let properties = request.into_inner().properties;
    if path_params.into_inner().instance_id != properties.id {
        return Err(HttpError::for_internal_error(
            "UUID mismatch (path did not match struct)".to_string(),
        ));
    }

    *context = Some(I::new(properties, &server_context.config)?);

    Ok(HttpResponseCreated(api::InstanceEnsureResponse {}))
}
/*
#[endpoint {
    method = GET,
    path = "/instances/{instance_id}",
}]
async fn instance_get(
    rqctx: Arc<RequestContext<ServerContext>>,
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
    method = PUT,
    path = "/instances/{instance_id}/state",
}]
async fn instance_state_put(
    rqctx: Arc<RequestContext<ServerContext>>,
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

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<ServerContext>>,
    path_params: Path<api::InstancePathParams>,
    request: TypedBody<api::InstanceSerialRequest>,
) -> Result<HttpResponseOk<api::InstanceSerialResponse>, HttpError> {
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

    // Backpressure: Wait until either the buffer has filled completely, or
    // (for partial reads) 10ms have elapsed. This prevents spinning on a
    // serial console which is emitting single bytes at a time.
    let _ = tokio::time::timeout(
        core::time::Duration::from_millis(10),
        context.serial.read_buffer_full(),
    )
    .await;

    // The buffer may or may not actually have any output - we've already
    // introduced our backpressure mechanism with the previous tokio timeout,
    // so don't bother blocking on this particular read.
    let mut output = [0u8; 4096];
    let n = futures::select! {
        result = context.serial.read(&mut output).fuse() => {
            result.map_err(|e| {
                HttpError::for_internal_error(format!(
                    "Cannot read from serial: {}",
                    e
                ))
            })?
        },
        default => { 0 }
    };

    let input = request.into_inner().bytes;
    if !input.is_empty() {
        context.serial.write_all(&input).await.map_err(|e| {
            HttpError::for_internal_error(format!(
                "Cannot write to serial: {}",
                e
            ))
        })?;
    }
    let response = api::InstanceSerialResponse { bytes: output[..n].to_vec() };
    Ok(HttpResponseOk(response))
}
*/
