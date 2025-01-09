// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines to expose a connection to an instance's serial port.

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use backend::ConsoleBackend;
use dropshot::WebsocketConnectionRaw;
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use propolis_api_types::InstanceSerialConsoleControlMessage;
use slog::{info, warn};
use tokio::{
    select,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::{
    tungstenite::{
        protocol::{frame::coding::CloseCode, CloseFrame},
        Message,
    },
    WebSocketStream,
};

pub(crate) mod backend;
pub(crate) mod history_buffer;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn serial_event_done() {}
    fn serial_event_read(b: u8) {}
    fn serial_event_console_disconnect() {}
    fn serial_event_ws_recv(len: usize) {}
    fn serial_event_ws_error() {}
    fn serial_event_ws_disconnect() {}
    fn serial_buffer_size(n: usize) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ClientId(u64);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    ReadWrite,
    ReadOnly,
}

struct ClientTask {
    hdl: JoinHandle<()>,
    control_tx: mpsc::Sender<InstanceSerialConsoleControlMessage>,
    done_tx: oneshot::Sender<()>,
}

#[derive(Default)]
struct ClientTasks {
    tasks: BTreeMap<ClientId, ClientTask>,
    next_id: u64,
    rw_client_id: Option<ClientId>,
}

impl ClientTasks {
    fn next_id(&mut self) -> ClientId {
        let id = self.next_id;
        self.next_id += 1;
        ClientId(id)
    }

    fn remove_by_id(&mut self, id: ClientId) {
        self.tasks.remove(&id);
        match self.rw_client_id {
            Some(existing_id) if existing_id == id => {
                self.rw_client_id = None;
            }
            _ => {}
        }
    }

    fn remove_rw_client(&mut self) -> Option<ClientTask> {
        if let Some(id) = self.rw_client_id.take() {
            self.tasks.remove(&id)
        } else {
            None
        }
    }
}

pub struct SerialConsoleManager {
    log: slog::Logger,
    backend: Arc<ConsoleBackend>,
    client_tasks: Arc<Mutex<ClientTasks>>,
}

impl SerialConsoleManager {
    pub fn new(log: slog::Logger, backend: Arc<ConsoleBackend>) -> Self {
        Self {
            log,
            backend,
            client_tasks: Arc::new(Mutex::new(ClientTasks::default())),
        }
    }

    pub async fn stop(self) {
        let tasks = {
            let mut guard = self.client_tasks.lock().unwrap();
            std::mem::take(&mut guard.tasks)
        };

        let mut tasks: Vec<_> = tasks.into_values().collect();
        for task in &mut tasks {
            let (tx, _rx) = oneshot::channel();
            let done_tx = std::mem::replace(&mut task.done_tx, tx);
            let _ = done_tx.send(());
        }

        futures::future::join_all(tasks.into_iter().map(|task| task.hdl)).await;
    }

    pub async fn connect(
        &self,
        ws: WebSocketStream<WebsocketConnectionRaw>,
        kind: ClientKind,
    ) {
        const SERIAL_CHANNEL_SIZE: usize = 256;
        let (console_tx, console_rx) = mpsc::channel(SERIAL_CHANNEL_SIZE);
        let (control_tx, control_rx) = mpsc::channel(1);
        let (task_start_tx, task_start_rx) = oneshot::channel();
        let (task_done_tx, task_done_rx) = oneshot::channel();
        let (permissions, discipline) = match kind {
            ClientKind::ReadWrite => (
                backend::Permissions::ReadWrite,
                backend::ReadWaitDiscipline::Block,
            ),
            ClientKind::ReadOnly => (
                backend::Permissions::ReadOnly,
                backend::ReadWaitDiscipline::Close,
            ),
        };

        let prev_rw_task;
        {
            let mut client_tasks = self.client_tasks.lock().unwrap();
            let client_id = client_tasks.next_id();
            prev_rw_task = if kind == ClientKind::ReadWrite {
                client_tasks.remove_rw_client()
            } else {
                None
            };

            let backend_hdl =
                self.backend.attach_client(console_tx, permissions, discipline);

            let ctx = SerialTaskContext {
                log: self.log.clone(),
                ws,
                backend_hdl,
                console_rx,
                control_rx,
                start_rx: task_start_rx,
                done_rx: task_done_rx,
                client_tasks: self.client_tasks.clone(),
                client_id,
            };

            let task = ClientTask {
                hdl: tokio::spawn(async move { serial_task(ctx).await }),
                control_tx,
                done_tx: task_done_tx,
            };

            client_tasks.tasks.insert(client_id, task);
            if kind == ClientKind::ReadWrite {
                assert!(client_tasks.rw_client_id.is_none());
                client_tasks.rw_client_id = Some(client_id);
            }
        }

        if let Some(task) = prev_rw_task {
            let _ = task.done_tx.send(());
            let _ = task.hdl.await;
        }

        task_start_tx
            .send(())
            .expect("new serial task shouldn't exit before starting");
    }

    pub async fn notify_migration(&self, destination: SocketAddr) {
        let from_start = self.backend.bytes_since_start() as u64;
        let clients: Vec<_> = {
            let client_tasks = self.client_tasks.lock().unwrap();
            client_tasks
                .tasks
                .values()
                .map(|client| client.control_tx.clone())
                .collect()
        };

        for client in clients {
            let _ = client
                .send(InstanceSerialConsoleControlMessage::Migrating {
                    destination,
                    from_start,
                })
                .await;
        }
    }
}

/// Context passed to every serial console task.
struct SerialTaskContext {
    /// The logger this task should use.
    log: slog::Logger,

    /// The websocket connection this task uses to communicate with its client.
    ws: WebSocketStream<WebsocketConnectionRaw>,

    /// A handle representing this task's connection to the console backend.
    backend_hdl: backend::ClientHandle,

    /// Receives output bytes from the guest.
    console_rx: mpsc::Receiver<u8>,

    /// Receives control commands that should be relayed to websocket clients
    /// (e.g. impending migrations).
    control_rx: mpsc::Receiver<InstanceSerialConsoleControlMessage>,

    start_rx: oneshot::Receiver<()>,

    /// Signaled when the task is
    done_rx: oneshot::Receiver<()>,

    /// A reference to the manager's client task map, used to deregister this
    /// task on disconnection.
    client_tasks: Arc<Mutex<ClientTasks>>,

    /// This task's ID, used to deregister this task on disconnection.
    client_id: ClientId,
}

async fn serial_task(
    SerialTaskContext {
        log,
        ws,
        mut backend_hdl,
        mut console_rx,
        mut control_rx,
        start_rx,
        mut done_rx,
        client_tasks,
        client_id,
    }: SerialTaskContext,
) {
    enum Event {
        Done,
        ConsoleRead(u8),
        ConsoleDisconnected,
        WroteToBackend(Result<usize, std::io::Error>),
        ControlMessage(InstanceSerialConsoleControlMessage),
        WebsocketMessage(Message),
        WebsocketError(tokio_tungstenite::tungstenite::Error),
        WebsocketDisconnected,
    }

    async fn close(
        log: &slog::Logger,
        client_id: ClientId,
        sink: SplitSink<WebSocketStream<WebsocketConnectionRaw>, Message>,
        stream: SplitStream<WebSocketStream<WebsocketConnectionRaw>>,
        reason: &str,
    ) {
        let mut ws =
            sink.reunite(stream).expect("sink and stream should match");
        if let Err(e) = ws
            .close(Some(CloseFrame {
                code: CloseCode::Away,
                reason: reason.into(),
            }))
            .await
        {
            warn!(
                log, "error sending close frame to client";
                "client_id" => client_id.0,
                "error" => ?e,
            );
        }
    }

    let _ = start_rx.await;

    info!(
        log,
        "serial console task started";
        "client_id" => client_id.0,
    );

    let mut remaining_to_send: VecDeque<u8> = VecDeque::new();
    let (mut sink, mut stream) = ws.split();
    let mut close_reason: Option<&'static str> = None;
    loop {
        use futures::future::Either;

        let (will_send, send_fut) =
            if backend_hdl.is_writable() && !remaining_to_send.is_empty() {
                remaining_to_send.make_contiguous();
                (
                    true,
                    Either::Left(
                        backend_hdl.write(remaining_to_send.as_slices().0),
                    ),
                )
            } else {
                (false, Either::Right(futures::future::pending()))
            };

        // If there are no bytes to be sent to the guest, accept another message
        // from the websocket.
        let ws_fut = if !will_send {
            Either::Left(stream.next())
        } else {
            Either::Right(futures::future::pending())
        };

        let event = select! {
            // The priority of these branches is important:
            //
            // 1. Requests to stop the client take precedence over everything
            //    else.
            // 2. New bytes written by the guest need to be processed before any
            //    other requests: if a guest outputs a byte while a read-write
            //    client is attached, the relevant vCPU will be blocked until
            //    the client processes the byte.
            biased;

            _ = &mut done_rx => {
                Event::Done
            }

            res = console_rx.recv() => {
                match res {
                    Some(b) => Event::ConsoleRead(b),
                    None => Event::ConsoleDisconnected,
                }
            }

            control = control_rx.recv() => {
                Event::ControlMessage(control.expect(
                    "serial control channel should outlive its task"
                ))
            }

            res = send_fut => {
                Event::WroteToBackend(res)
            }

            ws = ws_fut => {
                match ws {
                    None => Event::WebsocketDisconnected,
                    Some(Ok(msg)) => Event::WebsocketMessage(msg),
                    Some(Err(err)) => Event::WebsocketError(err),
                }
            }
        };

        match event {
            Event::Done => {
                probes::serial_event_done!(|| ());
                close_reason = Some("VM stopped");
                break;
            }
            Event::ConsoleRead(b) => {
                probes::serial_event_read!(|| (b));

                // Waiting outside the `select!` is OK here:
                //
                // - If the client is a read-write client, it is allowed to
                //   block the guest to ensure that every byte of guest output
                //   is transmitted to the client.
                // - If the client is a read-only client, and it is slow to
                //   acknowledge this message, its channel to the backend will
                //   eventually fill up. If this happens and the backend thus
                //   becomes unable to send new bytes, it will drop the channel
                //   to allow the guest to make progress.
                let _ = sink.send(Message::binary(vec![b])).await;
            }
            Event::ConsoleDisconnected => {
                probes::serial_event_console_disconnect!(|| ());
                info!(
                    log, "console backend dropped its client channel";
                    "client_id" => client_id.0
                );
                break;
            }
            Event::ControlMessage(control) => {
                let _ = sink
                    .send(Message::Text(
                        serde_json::to_string(&control).expect(
                            "control messages can always serialize into JSON",
                        ),
                    ))
                    .await;
            }
            Event::WroteToBackend(result) => {
                let written = match result {
                    Ok(n) => n,
                    Err(e) => {
                        let reason = if e.kind()
                            == std::io::ErrorKind::ConnectionAborted
                        {
                            "read-write console connection overtaken"
                        } else {
                            "error writing to console backend"
                        };

                        warn!(
                            log,
                            "dropping read-write console client";
                            "client_id" => client_id.0,
                            "error" => ?e,
                            "reason" => reason
                        );

                        close_reason = Some(reason);
                        break;
                    }
                };

                drop(remaining_to_send.drain(..written));
            }
            Event::WebsocketMessage(msg) => {
                match (backend_hdl.is_writable(), msg) {
                    (true, Message::Binary(bytes)) => {
                        probes::serial_event_ws_recv!(|| (bytes.len()));
                        remaining_to_send.extend(bytes.as_slice());
                    }
                    (false, Message::Binary(_)) => {
                        continue;
                    }
                    (_, _) => continue,
                }
            }
            Event::WebsocketError(e) => {
                probes::serial_event_ws_error!(|| ());
                warn!(
                    log, "serial console websocket error";
                    "client_id" => client_id.0,
                    "error" => ?e
                );
                break;
            }
            Event::WebsocketDisconnected => {
                probes::serial_event_ws_disconnect!(|| ());
                info!(
                    log, "serial console client disconnected";
                    "client_id" => client_id.0
                );
                break;
            }
        }
    }

    info!(log, "serial console task exiting"; "client_id" => client_id.0);
    if let Some(close_reason) = close_reason {
        close(&log, client_id, sink, stream, close_reason).await;
    }

    client_tasks.lock().unwrap().remove_by_id(client_id);
}
