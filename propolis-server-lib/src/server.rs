//! HTTP server callback functions.

use anyhow::Result;
use dropshot::{
    endpoint, ApiDescription, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, RequestContext, TypedBody,
};
use futures::FutureExt;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Mutex};

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
    serial: Serial<DispCtx, LpcUart>,
    state_watcher: watch::Receiver<StateChange>,
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
        PropolisState::Quiesce => ApiState::Stopped,
        PropolisState::Halt => ApiState::Stopped,
        PropolisState::Reset => ApiState::Stopped,
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
        serial: com1.unwrap(),
        state_watcher: rx,
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

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/serial",
}]
async fn instance_serial(
    rqctx: Arc<RequestContext<Context>>,
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

/// Returns a Dropshot [`ApiDescription`] object to launch a server.
pub fn api() -> ApiDescription<Context> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();
    api
}
