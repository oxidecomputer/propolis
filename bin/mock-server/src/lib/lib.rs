// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implementation of a mock Propolis server

use std::io::{Error as IoError, ErrorKind};
use std::sync::Arc;

use base64::Engine;
use dropshot::{
    channel, endpoint, ApiDescription, HttpError, HttpResponseCreated,
    HttpResponseOk, HttpResponseUpdatedNoContent, Query, RequestContext,
    TypedBody, WebsocketConnection,
};
use futures::SinkExt;
use slog::{error, info, Logger};
use thiserror::Error;
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

mod api_types;
mod copied;

use crate::copied::{slot_to_pci_path, SlotType};
use api_types::types as api;

#[derive(Debug, Eq, PartialEq, Error)]
pub enum Error {
    #[error("Failed to send simulated state change update through channel")]
    TransitionSendFail,
    #[error("Cannot request any new mock instance state once it is stopped/destroyed/failed")]
    TerminalState,
}

/// simulated instance properties
pub struct InstanceContext {
    pub state: api::InstanceState,
    pub generation: u64,
    pub properties: api::InstanceProperties,
    serial: Arc<serial::Serial>,
    serial_task: serial::SerialTask,
    state_watcher_rx: watch::Receiver<api::InstanceStateMonitorResponse>,
    state_watcher_tx: watch::Sender<api::InstanceStateMonitorResponse>,
}

impl InstanceContext {
    pub fn new(properties: api::InstanceProperties, _log: &Logger) -> Self {
        let (state_watcher_tx, state_watcher_rx) =
            watch::channel(api::InstanceStateMonitorResponse {
                gen: 0,
                state: api::InstanceState::Creating,
                migration: api::InstanceMigrateStatusResponse {
                    migration_in: None,
                    migration_out: None,
                },
            });
        let serial = serial::Serial::new(&properties.name);

        let serial_task = serial::SerialTask::spawn();

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
    pub async fn set_target_state(
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
                            state: self.state,
                            migration: api::InstanceMigrateStatusResponse {
                                migration_in: None,
                                migration_out: None,
                            },
                        })
                        .map_err(|_| Error::TransitionSendFail)
                }
                api::InstanceStateRequested::Stop => {
                    self.state = api::InstanceState::Stopped;
                    self.serial_task.shutdown().await;
                    Ok(())
                }
            },
        }
    }
}

/// Contextual information accessible from mock HTTP callbacks.
pub struct Context {
    instance: Mutex<Option<InstanceContext>>,
    log: Logger,
}

impl Context {
    pub fn new(log: Logger) -> Self {
        Context { instance: Mutex::new(None), log }
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
            .decode(cloud_init_bytes)
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
        state: instance.state,
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
                migration: api::InstanceMigrateStatusResponse {
                    migration_in: None,
                    migration_out: None,
                },
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
    instance.set_target_state(requested_state).await.map_err(|err| {
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
    query: Query<api_types::InstanceSerialParams>,
    websock: WebsocketConnection,
) -> dropshot::WebsocketChannelResult {
    let config = WebSocketConfig::default();
    let mut ws_stream = WebSocketStream::from_raw_socket(
        websock.into_inner(),
        Role::Server,
        Some(config),
    )
    .await;

    match rqctx.context().instance.lock().await.as_ref() {
        None => {
            ws_stream.send(Message::Close(None)).await?;
            Err("Instance not yet created!".into())
        }
        Some(InstanceContext { state, .. })
            if *state != api::InstanceState::Running =>
        {
            ws_stream.send(Message::Close(None)).await?;
            Err(format!("Instance isn't Running! ({:?})", state).into())
        }
        Some(instance_ctx) => {
            let serial = instance_ctx.serial.clone();

            let query_params = query.into_inner();
            let history_query = serial::HistoryQuery::from_query(
                query_params.from_start,
                query_params.most_recent,
            );
            if let Some(mut hq) = history_query {
                loop {
                    let (data, offset) = serial.history_vec(hq, None).await?;
                    if data.is_empty() {
                        break;
                    }
                    ws_stream.send(Message::Binary(data)).await?;
                    hq = serial::HistoryQuery::FromStart(offset);
                }
            }
            instance_ctx.serial_task.new_conn(ws_stream).await;
            Ok(())
        }
    }
}

#[endpoint {
    method = GET,
    path = "/instance/serial/history",
}]
async fn instance_serial_history_get(
    rqctx: RequestContext<Arc<Context>>,
    query: Query<api_types::InstanceSerialHistoryParams>,
) -> Result<HttpResponseOk<api::InstanceSerialConsoleHistoryResponse>, HttpError>
{
    let query_params = query.into_inner();

    let history_query = serial::HistoryQuery::from_query(
        query_params.from_start,
        query_params.most_recent,
    )
    .ok_or_else(|| {
        HttpError::for_bad_request(
            None,
            "Exactly one of 'from_start' or 'most_recent' must be specified."
                .to_string(),
        )
    })?;
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
        .history_vec(history_query, max_bytes)
        .await
        .map_err(|e| HttpError::for_bad_request(None, e.to_string()))?;

    Ok(HttpResponseOk(api::InstanceSerialConsoleHistoryResponse {
        data,
        last_byte_offset: end as u64,
    }))
}

mod serial {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use futures::StreamExt;
    use hyper::upgrade::Upgraded;
    use tokio::sync::{mpsc, Notify};
    use tokio_tungstenite::tungstenite::protocol::{
        frame::coding::CloseCode, CloseFrame,
    };
    use tokio_tungstenite::WebSocketStream;

    type WsConn = WebSocketStream<Upgraded>;

    const DEFAULT_MAX_LEN: usize = 1024;

    pub(crate) enum HistoryQuery {
        FromStart(usize),
        MostRecent(usize),
    }
    impl HistoryQuery {
        pub(crate) const fn from_query(
            from_start: Option<u64>,
            most_recent: Option<u64>,
        ) -> Option<Self> {
            match (from_start, most_recent) {
                (Some(from_start), None) => {
                    Some(Self::FromStart(from_start as usize))
                }
                (None, Some(most_recent)) => {
                    Some(Self::MostRecent(most_recent as usize))
                }
                _ => None,
            }
        }
    }

    /// Fake serial task
    pub(crate) struct SerialTask {
        chan_ctrl: mpsc::Sender<()>,
        chan_ws: mpsc::Sender<WsConn>,
        is_shutdown: AtomicBool,
    }
    impl SerialTask {
        pub fn spawn() -> Self {
            let (ctrl_send, ctrl_recv) = mpsc::channel(1);
            let (ws_send, ws_recv) = mpsc::channel::<WsConn>(1);

            tokio::spawn(async move {
                Self::serial_task_work(ctrl_recv, ws_recv).await
            });
            Self {
                chan_ctrl: ctrl_send,
                chan_ws: ws_send,
                is_shutdown: AtomicBool::new(false),
            }
        }

        /// Drive client connections to the UART websocket
        ///
        /// At this time, there is no real data being emitted from the mock
        /// instance besides what's made up in the [`Serial`] below.  Because of
        /// that, the serial task has little to do besides holding the websocket
        /// connections open until the mock instance enters shutdown.
        async fn serial_task_work(
            mut chan_ctrl: mpsc::Receiver<()>,
            mut chan_ws: mpsc::Receiver<WsConn>,
        ) {
            let bail = Notify::new();
            let mut connections = futures::stream::FuturesUnordered::new();
            let mut is_shutdown = false;

            /// Send appropriate shutdown notice
            async fn close_for_shutdown(mut conn: WsConn) {
                let _ = conn
                    .close(Some(CloseFrame {
                        code: CloseCode::Away,
                        reason: "VM stopped".into(),
                    }))
                    .await;
            }

            /// Wait for a client connection to close (while discarding any
            /// input from it), or a signal that the VM is shutting down.
            async fn wait_for_close(
                mut conn: WsConn,
                bail: &Notify,
            ) -> Option<WsConn> {
                let mut pconn = std::pin::Pin::new(&mut conn);

                loop {
                    tokio::select! {
                        msg = pconn.next() => {
                            // Discard input (if any) and keep truckin'
                            msg.as_ref()?;
                        },
                        _ = bail.notified() => {
                            return Some(conn);
                        }
                    }
                }
            }

            loop {
                tokio::select! {
                    _vm_shutdown = chan_ctrl.recv() => {
                        // We've been signaled that the VM is shutdown
                        bail.notify_waiters();
                        chan_ws.close();
                        is_shutdown = true;
                        if connections.is_empty() {
                            return;
                        }
                    }
                    conn = chan_ws.recv() => {
                        // A new client connection has been passed to us
                        if conn.is_none() {
                            continue;
                        }
                        let conn = conn.unwrap();
                        if is_shutdown {
                            close_for_shutdown(conn).await;
                            continue;
                        }
                        connections
                            .push(async { wait_for_close(conn, &bail).await });
                    }
                    disconnect = connections.next(), if !connections.is_empty() => {
                        match disconnect {
                            None => {
                                // last open client
                                assert!(connections.is_empty());
                                if is_shutdown {
                                    return;
                                }
                            }
                            Some(Some(conn)) => {
                                // client needs disconnect due to shutdown
                                close_for_shutdown(conn).await;
                            }
                            _ => {
                                // client disconnected itself
                                continue
                            }
                        }
                    }
                }
            }
        }

        pub async fn new_conn(&self, ws: WsConn) {
            if let Err(mut ws) = self.chan_ws.send(ws).await.map_err(|e| e.0) {
                let _ = ws
                    .close(Some(CloseFrame {
                        code: CloseCode::Away,
                        reason: "VM stopped".into(),
                    }))
                    .await;
            }
        }

        pub async fn shutdown(&self) {
            if !self.is_shutdown.swap(true, Ordering::Relaxed) {
                self.chan_ctrl.send(()).await.unwrap();
            }
        }
    }

    /// Mock source of UART data from the guest, including history
    pub(crate) struct Serial {
        mock_data: Vec<u8>,
    }
    impl Serial {
        pub(super) fn new(name: &str) -> Arc<Self> {
            Arc::new(Self { mock_data: Self::mock_data(name) })
        }

        // Conjure up some fake console output
        fn mock_data(name: &str) -> Vec<u8> {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};

            let mut buf = Vec::with_capacity(1024);
            #[rustfmt::skip]
            let gerunds = [
                "Loading", "Reloading", "Advancing", "Reticulating",
                "Defeating", "Spoiling", "Cooking", "Destroying", "Resenting",
                "Introducing", "Reiterating", "Blasting", "Tolling",
                "Delivering", "Engendering", "Establishing",
            ];
            #[rustfmt::skip]
            let nouns = [
                "canon", "browsers", "meta", "splines", "villains", "plot",
                "books", "evidence", "decisions", "chaos", "points",
                "processors", "bells", "value", "gender", "shots",
            ];
            let mut hasher = DefaultHasher::new();
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
            buf
        }

        pub async fn history_vec(
            &self,
            query: HistoryQuery,
            max_bytes: Option<usize>,
        ) -> Result<(Vec<u8>, usize), &'static str> {
            let end = self.mock_data.len();
            let byte_limit = max_bytes.unwrap_or(DEFAULT_MAX_LEN);

            match query {
                HistoryQuery::FromStart(n) => {
                    if n > self.mock_data.len() {
                        Err("requesting data beyond history")
                    } else {
                        let data = &self.mock_data[n..];
                        let truncated = &data[..(data.len().min(byte_limit))];
                        Ok((truncated.to_vec(), end))
                    }
                }
                HistoryQuery::MostRecent(n) => {
                    let clamped = n.min(self.mock_data.len());
                    let data =
                        &self.mock_data[(self.mock_data.len() - clamped)..];
                    let truncated = &data[..(data.len().min(byte_limit))];
                    Ok((truncated.to_vec(), end))
                }
            }
        }
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
