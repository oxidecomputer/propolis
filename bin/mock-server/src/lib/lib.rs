// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implementation of a mock Propolis server

use std::sync::Arc;

use dropshot::{
    channel, endpoint, ApiDescription, HttpError, HttpResponseCreated,
    HttpResponseOk, HttpResponseUpdatedNoContent, Query, RequestContext,
    TypedBody, WebsocketConnection,
};
use futures::SinkExt;
use slog::{error, o, Logger};
use std::collections::BTreeMap;
use thiserror::Error;
use tokio::sync::{watch, Mutex};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

mod api_types;
use api_types::types::{self as api, InstanceEnsureRequest};

#[derive(Debug, Eq, PartialEq, Error)]
pub enum Error {
    #[error("Failed to send simulated state change update through channel")]
    TransitionSendFail,
    #[error("Cannot request any new mock instance state once it is stopped/destroyed/failed")]
    TerminalState,
    #[error("Cannot transition to {requested:?} from {current:?}")]
    InvalidTransition {
        current: api::InstanceState,
        requested: api::InstanceStateRequested,
    },
}

/// simulated instance properties
pub struct InstanceContext {
    pub state: api::InstanceState,
    pub generation: u64,
    pub properties: api::InstanceProperties,
    serial: Arc<serial::Serial>,
    serial_task: serial::SerialTask,
    state_watcher_rx:
        watch::Receiver<BTreeMap<u64, api::InstanceStateMonitorResponse>>,
    state_watcher_tx:
        watch::Sender<BTreeMap<u64, api::InstanceStateMonitorResponse>>,
}

impl InstanceContext {
    pub fn new(properties: api::InstanceProperties, _log: &Logger) -> Self {
        let (state_watcher_tx, state_watcher_rx) = {
            let mut states = BTreeMap::new();
            states.insert(
                0,
                api::InstanceStateMonitorResponse {
                    gen: 0,
                    state: api::InstanceState::Creating,
                    migration: api::InstanceMigrateStatusResponse {
                        migration_in: None,
                        migration_out: None,
                    },
                },
            );
            watch::channel(states)
        };
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
        log: &Logger,
        target: api::InstanceStateRequested,
    ) -> Result<(), Error> {
        match (self.state, target) {
            (
                api::InstanceState::Stopped
                | api::InstanceState::Destroyed
                | api::InstanceState::Failed,
                _,
            ) => {
                // Cannot request any state once the target is halt/destroy
                Err(Error::TerminalState)
            }
            (
                api::InstanceState::Rebooting,
                api::InstanceStateRequested::Run,
            ) => {
                // Requesting a run when already on the road to reboot is an
                // immediate success.
                Ok(())
            }
            (api::InstanceState::Running, api::InstanceStateRequested::Run) => {
                Ok(())
            }
            (
                api::InstanceState::Running,
                api::InstanceStateRequested::Reboot,
            ) => {
                self.queue_states(
                    log,
                    &[
                        api::InstanceState::Rebooting,
                        api::InstanceState::Running,
                    ],
                )
                .await;
                Ok(())
            }
            (current, api::InstanceStateRequested::Reboot) => {
                Err(Error::InvalidTransition {
                    current,
                    requested: api::InstanceStateRequested::Reboot,
                })
            }
            (_, api::InstanceStateRequested::Run) => {
                self.queue_states(log, &[api::InstanceState::Running]).await;
                Ok(())
            }
            (
                api::InstanceState::Stopping,
                api::InstanceStateRequested::Stop,
            ) => Ok(()),
            (_, api::InstanceStateRequested::Stop) => {
                self.queue_states(
                    log,
                    &[
                        api::InstanceState::Stopping,
                        api::InstanceState::Stopped,
                    ],
                )
                .await;
                self.serial_task.shutdown().await;
                Ok(())
            }
        }
    }

    async fn queue_states(
        &mut self,
        log: &Logger,
        states: &[api::InstanceState],
    ) {
        self.state_watcher_tx.send_modify(|queue| {
            for &state in states {
                self.generation += 1;
                self.state = state;
                queue.insert(self.generation, api::InstanceStateMonitorResponse { gen: self.generation, migration: api::InstanceMigrateStatusResponse { migration_in: None, migration_out: None }, state });
                slog::info!(log, "queued instance state transition"; "state" => ?state, "gen" => ?self.generation);
            }
        })
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
    let InstanceEnsureRequest { properties, .. } = request;

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
        let states = state_watcher.borrow().clone();
        if let Some(state) = states.get(&(gen + 1)) {
            return Ok(HttpResponseOk(state.clone()));
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
    instance.set_target_state(&rqctx.log, requested_state).await.map_err(
        |err| {
            HttpError::for_internal_error(format!(
                "Failed to transition: {}",
                err
            ))
        },
    )?;
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

    use dropshot::WebsocketConnectionRaw;
    use futures::StreamExt;
    use tokio::sync::{mpsc, Notify};
    use tokio_tungstenite::tungstenite::protocol::{
        frame::coding::CloseCode, CloseFrame,
    };
    use tokio_tungstenite::WebSocketStream;

    type WsConn = WebSocketStream<WebsocketConnectionRaw>;

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
///
/// This function should be avoided in favor of `start()` because using this
/// function requires that the consumer and Propolis update Dropshot
/// dependencies in lockstep due to the sharing of various types.
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

// These types need to be exposed so that consumers have names for them without
// having to maintain a dropshot dependency in lockstep with their dependency on
// this crate.

/// configuration for the dropshot server
pub type Config = dropshot::ConfigDropshot;
/// the dropshot server itself
pub type Server = dropshot::HttpServer<Arc<Context>>;
/// errors returned from attempting to start a dropshot server
// Dropshot should expose this, but it's going to be removed anyway.
pub type ServerStartError = Box<dyn std::error::Error + Send + Sync>;

/// Starts a Propolis mock server
pub fn start(config: Config, log: Logger) -> Result<Server, ServerStartError> {
    let propolis_log = log.new(o!("component" => "propolis-server-mock"));
    let dropshot_log = log.new(o!("component" => "dropshot"));
    let private = Arc::new(Context::new(propolis_log));
    let starter = dropshot::HttpServerStarter::new(
        &config,
        api(),
        private,
        &dropshot_log,
    )?;
    Ok(starter.start())
}
