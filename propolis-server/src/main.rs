use anyhow::{anyhow, Result};
use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpError, HttpResponseCreated, HttpResponseOk,
    HttpResponseUpdatedNoContent, HttpServerStarter, Path, RequestContext,
    TypedBody,
};
use std::io::{Error, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use structopt::StructOpt;

use propolis::dispatch::DispCtx;
use propolis::hw::chipset::Chipset;
use propolis::hw::pci;
use propolis::hw::uart::LpcUart;
use propolis::instance::Instance;

use tokio::sync::Mutex;
use tokio::io::{AsyncWriteExt, AsyncReadExt};

mod api;
mod config;
mod initializer;
mod serial;

use initializer::{build_instance, MachineInitializer};
use serial::Serial;

// TODO(error) Do a pass of HTTP codes (error and ok)
// TODO(idempotency) Idempotency mechanisms?

// All context for a single propolis instance.
struct InstanceContext {
    // The instance, which may or may not be instantiated.
    instance: Arc<Instance>,
    properties: api::InstanceProperties,
    serial: Serial<DispCtx, LpcUart>,
}

struct ServerContext {
    context: Mutex<Option<InstanceContext>>,
    config: config::Config,
}

impl ServerContext {
    fn new(config: config::Config) -> Self {
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
async fn instance_ensure(
    rqctx: Arc<RequestContext<ServerContext>>,
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

    // Create the instance.
    let lowmem = (properties.memory * 1024 * 1024) as usize;
    let instance = build_instance(&properties.name, properties.vcpus, lowmem)
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
            Ok(())
        })
        .map_err(|err| {
            HttpError::for_internal_error(format!(
                "Failed to initialize machine: {}",
                err.to_string()
            ))
        })?;

    instance.print();
    instance.on_transition(Box::new(|next_state| {
        println!("state cb: {:?}", next_state);
    }));

    // Save the newly created instance in the server's context.
    *context =
        Some(InstanceContext { instance, properties, serial: com1.unwrap() });

    Ok(HttpResponseCreated(api::InstanceEnsureResponse {}))
}

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

    // Attempt to read from the serial console, but stop trying to read after
    // 50ms if there is nothing to observe.
    //
    // NOTE: When this interface is converted to websockets, this timeout will
    // be unnecessary - the connection can just block on the read, since
    // reading/writing will be decoupled.
    let mut output = [0u8; 4096];
    let n = {
        match tokio::time::timeout(
            core::time::Duration::from_millis(50),
            context.serial.read(&mut output)
        ).await {
            Ok(result) => {
                // The read completed without a timeout firing.
                let n = result.map_err(|e| {
                    HttpError::for_internal_error(format!("Cannot read from serial: {}", e))
                })?;
                n
            }
            Err(_) => {0}
        }
    };

    let input = request.into_inner().bytes;
    if input.len() != 0 {
        context.serial.write_all(&input).await.map_err(|e| {
            HttpError::for_internal_error(format!("Cannot write to serial: {}", e))
        })?;
    }
    let response = api::InstanceSerialResponse { bytes: output[..n].to_vec() };
    Ok(HttpResponseOk(response))
}

#[derive(Debug, StructOpt)]
#[structopt(
    name = "propolis-server",
    about = "An HTTP server providing access to Propolis"
)]
struct Opt {
    #[structopt(parse(from_os_str))]
    cfg: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Command line arguments.
    let opt = Opt::from_args();
    let config = config::parse(&opt.cfg)?;

    // Dropshot configuration.
    let config_dropshot: ConfigDropshot = Default::default();
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("propolis-server")
        .map_err(|error| anyhow!("failed to create logger: {}", error))?;

    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();

    let context = ServerContext::new(config);
    let server = HttpServerStarter::new(&config_dropshot, api, context, &log)
        .map_err(|error| anyhow!("Failed to start server: {}", error))?
        .start();
    server.await.map_err(|e| anyhow!("Server exited with an error: {}", e))
}
