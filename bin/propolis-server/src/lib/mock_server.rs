//! Implementation of a mock Propolis server

use base64::Engine;
use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::HttpError;
use dropshot::HttpResponseCreated;
use dropshot::HttpResponseOk;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::RequestContext;
use dropshot::TypedBody;
use dropshot::WebsocketConnection;
use dropshot::{channel, Query};
use futures::SinkExt;
use slog::{error, info, Logger};
use std::collections::VecDeque;
use std::convert::TryFrom;
use std::hash::{Hash, Hasher};
use std::io::{Error as IoError, ErrorKind};
use std::num::NonZeroUsize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{protocol::Role, Message};
use tokio_tungstenite::WebSocketStream;

#[derive(Debug, Eq, PartialEq, Error)]
pub enum Error {
    #[error("Failed to send simulated state change update through channel")]
    TransitionSendFail,
    #[error("Cannot request any new mock instance state once it is stopped/destroyed/failed")]
    TerminalState,
}

use propolis::chardev;
use propolis::chardev::{SinkNotifier, SourceNotifier};
use propolis_client::handmade::api;

use crate::config::Config;
use crate::serial::history_buffer::SerialHistoryOffset;
use crate::serial::{Serial, SerialTask};
use crate::spec::{slot_to_pci_path, SlotType};

/// simulated instance properties
pub struct InstanceContext {
    pub state: api::InstanceState,
    pub generation: u64,
    pub properties: api::InstanceProperties,
    serial: Arc<Serial<MockUart>>,
    serial_task: SerialTask,
    state_watcher_rx: watch::Receiver<api::InstanceStateMonitorResponse>,
    state_watcher_tx: watch::Sender<api::InstanceStateMonitorResponse>,
}

impl InstanceContext {
    pub fn new(properties: api::InstanceProperties, log: &Logger) -> Self {
        let (state_watcher_tx, state_watcher_rx) =
            watch::channel(api::InstanceStateMonitorResponse {
                gen: 0,
                state: api::InstanceState::Creating,
            });
        let mock_uart = Arc::new(MockUart::new(&properties.name));
        let sink_size = NonZeroUsize::new(64).unwrap();
        let source_size = NonZeroUsize::new(1024).unwrap();
        let serial = Arc::new(Serial::new(mock_uart, sink_size, source_size));
        let serial_clone = serial.clone();

        let (websocks_ch, websocks_recv) = mpsc::channel(1);
        let (close_ch, close_recv) = oneshot::channel();

        let log = log.new(slog::o!("component" => "serial task"));
        let task = tokio::spawn(async move {
            if let Err(e) = super::serial::instance_serial_task(
                websocks_recv,
                close_recv,
                serial_clone,
                log.clone(),
            )
            .await
            {
                error!(log, "Spawning serial task failed: {}", e);
            }
        });

        let serial_task = SerialTask { task, close_ch, websocks_ch };

        Self {
            state: api::InstanceState::Creating,
            generation: 0,
            properties,
            serial,
            serial_task,
            state_watcher_rx,
            state_watcher_tx,
        }
    }

    /// Updates the state of the mock instance.
    ///
    /// Returns an error if the state transition is invalid.
    pub fn set_target_state(
        &mut self,
        target: api::InstanceStateRequested,
    ) -> Result<(), Error> {
        match self.state {
            api::InstanceState::Stopped
            | api::InstanceState::Destroyed
            | api::InstanceState::Failed => {
                // Cannot request any state once the target is halt/destroy
                Err(Error::TerminalState)
            }
            api::InstanceState::Rebooting
                if matches!(target, api::InstanceStateRequested::Run) =>
            {
                // Requesting a run when already on the road to reboot is an
                // immediate success.
                Ok(())
            }
            _ => match target {
                api::InstanceStateRequested::Run
                | api::InstanceStateRequested::Reboot => {
                    self.generation += 1;
                    self.state = api::InstanceState::Running;
                    self.state_watcher_tx
                        .send(api::InstanceStateMonitorResponse {
                            gen: self.generation,
                            state: self.state.clone(),
                        })
                        .map_err(|_| Error::TransitionSendFail)
                }
                api::InstanceStateRequested::Stop => {
                    self.state = api::InstanceState::Stopped;
                    Ok(())
                }
                api::InstanceStateRequested::MigrateStart => {
                    unimplemented!("migration not yet implemented")
                }
            },
        }
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
    path = "/instance",
}]
async fn instance_ensure(
    rqctx: RequestContext<Arc<Context>>,
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

    // Handle an already-initialized instance
    let mut instance = server_context.instance.lock().await;
    if let Some(instance) = &*instance {
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
        slot_to_pci_path(nic.slot, SlotType::Nic).map_err(|e| {
            let err = IoError::new(
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
        slot_to_pci_path(disk.slot, SlotType::Disk).map_err(|e| {
            let err = IoError::new(
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
        slot_to_pci_path(api::Slot(0), SlotType::CloudInit).map_err(|e| {
            let err = IoError::new(ErrorKind::InvalidData, e.to_string());
            HttpError::for_internal_error(format!(
                "Cannot build instance: {}",
                err
            ))
        })?;
        base64::engine::general_purpose::STANDARD
            .decode(&cloud_init_bytes)
            .map_err(|e| {
                let err = IoError::new(ErrorKind::InvalidInput, e.to_string());
                HttpError::for_internal_error(format!(
                    "Cannot build instance: {}",
                    err
                ))
            })?;
        info!(server_context.log, "cloud-init disk created");
    }

    *instance = Some(InstanceContext::new(properties, &server_context.log));
    Ok(HttpResponseCreated(api::InstanceEnsureResponse { migrate: None }))
}

#[endpoint {
    method = GET,
    path = "/instance",
}]
async fn instance_get(
    rqctx: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<api::InstanceGetResponse>, HttpError> {
    let instance = rqctx.context().instance.lock().await;
    let instance = instance.as_ref().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;
    let instance_info = api::Instance {
        properties: instance.properties.clone(),
        state: instance.state.clone(),
        disks: vec![],
        nics: vec![],
    };
    Ok(HttpResponseOk(api::InstanceGetResponse { instance: instance_info }))
}

#[endpoint {
    method = GET,
    path = "/instance/state-monitor",
}]
async fn instance_state_monitor(
    rqctx: RequestContext<Arc<Context>>,
    request: TypedBody<api::InstanceStateMonitorRequest>,
) -> Result<HttpResponseOk<api::InstanceStateMonitorResponse>, HttpError> {
    let (mut state_watcher, gen) = {
        let instance = rqctx.context().instance.lock().await;
        let instance = instance.as_ref().ok_or_else(|| {
            HttpError::for_internal_error(
                "Server not initialized (no instance)".to_string(),
            )
        })?;
        let gen = request.into_inner().gen;
        let state_watcher = instance.state_watcher_rx.clone();
        (state_watcher, gen)
    };

    loop {
        let last = state_watcher.borrow().clone();
        if gen <= last.gen {
            let response = api::InstanceStateMonitorResponse {
                gen: last.gen,
                state: last.state,
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
    rqctx: RequestContext<Arc<Context>>,
    request: TypedBody<api::InstanceStateRequested>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let mut instance = rqctx.context().instance.lock().await;
    let instance = instance.as_mut().ok_or_else(|| {
        HttpError::for_internal_error(
            "Server not initialized (no instance)".to_string(),
        )
    })?;
    let requested_state = request.into_inner();
    instance.set_target_state(requested_state).map_err(|err| {
        HttpError::for_internal_error(format!("Failed to transition: {}", err))
    })?;
    Ok(HttpResponseUpdatedNoContent {})
}

// TODO: mock the "Serial" struct itself instead?
#[channel {
    protocol = WEBSOCKETS,
    path = "/instance/serial",
}]
async fn instance_serial(
    rqctx: RequestContext<Arc<Context>>,
    query: Query<api::InstanceSerialConsoleStreamRequest>,
    websock: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let config =
        WebSocketConfig { max_send_queue: Some(4096), ..Default::default() };
    let mut ws_stream = WebSocketStream::from_raw_socket(
        websock.into_inner(),
        Role::Server,
        Some(config),
    )
    .await;

    let instance_mtx = rqctx.context().instance.lock().await;
    if instance_mtx.as_ref().unwrap().state != api::InstanceState::Running {
        ws_stream.send(Message::Close(None)).await?;
        return Err("Instance isn't running!".into());
    }

    let serial = instance_mtx.as_ref().unwrap().serial.clone();

    let byte_offset = SerialHistoryOffset::try_from(&query.into_inner()).ok();
    if let Some(mut byte_offset) = byte_offset {
        loop {
            let (data, offset) = serial.history_vec(byte_offset, None).await?;
            if data.is_empty() {
                break;
            }
            ws_stream.send(Message::Binary(data)).await?;
            byte_offset = SerialHistoryOffset::FromStart(offset);
        }
    }

    instance_mtx
        .as_ref()
        .unwrap()
        .serial_task
        .websocks_ch
        .send(ws_stream)
        .await
        .map_err(|e| format!("Serial socket hand-off failed: {}", e).into())
}

#[endpoint {
    method = GET,
    path = "/instance/serial/history",
}]
async fn instance_serial_history_get(
    rqctx: RequestContext<Arc<Context>>,
    query: Query<api::InstanceSerialConsoleHistoryRequest>,
) -> Result<HttpResponseOk<api::InstanceSerialConsoleHistoryResponse>, HttpError>
{
    let query_params = query.into_inner();
    let byte_offset = SerialHistoryOffset::try_from(&query_params)?;
    let max_bytes = query_params.max_bytes.map(|x| x as usize);

    let ctx = rqctx.context();
    let (data, end) = ctx
        .instance
        .lock()
        .await
        .as_ref()
        .ok_or(HttpError::for_internal_error(
            "No mock instance instantiated".to_string(),
        ))?
        .serial
        .history_vec(byte_offset, max_bytes)
        .await
        .map_err(|e| HttpError::for_bad_request(None, e.to_string()))?;

    Ok(HttpResponseOk(api::InstanceSerialConsoleHistoryResponse {
        data,
        last_byte_offset: end as u64,
    }))
}

// (ahem) mock *thou* art.
struct MockUart {
    buf: std::sync::Mutex<VecDeque<u8>>,
}

impl MockUart {
    fn new(name: &str) -> Self {
        let mut buf = VecDeque::with_capacity(1024);
        #[rustfmt::skip]
        let gerunds = [
            "Loading", "Reloading", "Advancing", "Reticulating", "Defeating",
            "Spoiling", "Cooking", "Destroying", "Resenting", "Introducing",
            "Reiterating", "Blasting", "Tolling", "Delivering", "Engendering",
            "Establishing",
        ];
        #[rustfmt::skip]
        let nouns = [
            "canon", "browsers", "meta", "splines", "villains",
            "plot", "books", "evidence", "decisions", "chaos",
            "points", "processors", "bells", "value", "gender",
            "shots",
        ];
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        let mut entropy = hasher.finish();
        buf.extend(
            format!(
                "This is simulated serial console output for {}.\r\n",
                name
            )
            .as_bytes(),
        );
        while entropy != 0 {
            let gerund = gerunds[entropy as usize % gerunds.len()];
            entropy /= gerunds.len() as u64;
            let noun = nouns[entropy as usize % nouns.len()];
            entropy /= nouns.len() as u64;
            buf.extend(
                format!(
                    "{} {}... {}[\x1b[92m 0K \x1b[m]\r\n",
                    gerund,
                    noun,
                    " ".repeat(40 - gerund.len() - noun.len())
                )
                .as_bytes(),
            );
        }
        buf.extend(
            format!(
                "\x1b[2J\x1b[HOS/478 ({name}) (ttyl)\r\n\r\n{name} login: ",
                name = name
            )
            .as_bytes(),
        );
        Self { buf: std::sync::Mutex::new(buf) }
    }
}

impl chardev::Sink for MockUart {
    fn write(&self, data: u8) -> bool {
        self.buf.lock().unwrap().push_back(data);
        true
    }

    fn set_notifier(&self, _f: Option<SinkNotifier>) {
        //todo!()
    }
}

impl chardev::Source for MockUart {
    fn read(&self) -> Option<u8> {
        self.buf.lock().unwrap().pop_front()
    }

    fn discard(&self, count: usize) -> usize {
        let mut buf = self.buf.lock().unwrap();
        let end = buf.len().min(count);
        buf.drain(0..end).count()
    }

    fn set_autodiscard(&self, _active: bool) {
        //todo!()
    }

    fn set_notifier(&self, _f: Option<SourceNotifier>) {
        //todo!()
    }
}

/// Returns a Dropshot [`ApiDescription`] object to launch a mock Propolis
/// server.
pub fn api() -> ApiDescription<Arc<Context>> {
    let mut api = ApiDescription::new();
    api.register(instance_ensure).unwrap();
    api.register(instance_get).unwrap();
    api.register(instance_state_monitor).unwrap();
    api.register(instance_state_put).unwrap();
    api.register(instance_serial).unwrap();
    api.register(instance_serial_history_get).unwrap();
    api
}
