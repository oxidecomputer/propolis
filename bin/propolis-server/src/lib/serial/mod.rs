// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Defines the [`SerialConsoleManager`], which manages client websocket
//! connections to one of an instance's serial ports.
//!
//! Each `SerialConsoleManager` brokers connections to one serial device. Each
//! websocket connection to the device creates a [`ClientTask`] tracked by the
//! manager. This task largely plumbs incoming bytes from the websocket into the
//! backend and vice-versa, but it can also be used to send control messages to
//! clients, e.g. to notify them that an instance is migrating.
//!
//! The connection manager implements the connection policies described in RFD
//! 491:
//!
//! - A single serial device can have only one read-write client at a time (but
//!   can have multiple read-only clients).
//! - The connection manager configures its connections to the backend so that
//!   - The backend will block when sending bytes to read-write clients that
//!     can't receive them immediately, but
//!   - The backend will disconnect read-only clients who can't immediately
//!     receive new bytes.

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use backend::ConsoleBackend;
use dropshot::WebsocketConnectionRaw;
use futures::{SinkExt, StreamExt};
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

/// Specifies whether a client should be a read-write or read-only client.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    ReadWrite,
    ReadOnly,
}

/// Tracks an individual client of this console and its associated tokio task.
struct ClientTask {
    /// The join handle for this client's task.
    hdl: JoinHandle<()>,

    /// Receives server-level commands and notifications about this connection.
    control_tx: mpsc::Sender<InstanceSerialConsoleControlMessage>,

    /// Triggered when the serial console manager shuts down.
    done_tx: oneshot::Sender<()>,
}

/// The collection of [`ClientTask`]s belonging to a console manager.
#[derive(Default)]
struct ClientTasks {
    /// A table mapping task IDs to task structs.
    tasks: BTreeMap<ClientId, ClientTask>,

    /// The next task ID to assign.
    next_id: u64,

    /// The ID of the current read-write client, if there is one.
    rw_client_id: Option<ClientId>,
}

impl ClientTasks {
    fn next_id(&mut self) -> ClientId {
        let id = self.next_id;
        self.next_id += 1;
        ClientId(id)
    }

    /// Removes the task with ID `id` from the task collection.
    fn remove_by_id(&mut self, id: ClientId) {
        self.tasks.remove(&id);
        match self.rw_client_id {
            Some(existing_id) if existing_id == id => {
                self.rw_client_id = None;
            }
            _ => {}
        }
    }

    /// If there is a read-write client in the task collection, removes it (from
    /// both the read-write client position and the task table) and returns it.
    fn remove_rw_client(&mut self) -> Option<ClientTask> {
        if let Some(id) = self.rw_client_id.take() {
            self.tasks.remove(&id)
        } else {
            None
        }
    }
}

/// Manages individual websocket client connections to a single serial device.
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

    /// Directs all client tasks to stop and waits for them all to finish.
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

    /// Creates a new task that connects a websocket stream to this manager's
    /// serial console.
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
                backend::FullReadChannelDiscipline::Block,
            ),
            ClientKind::ReadOnly => (
                backend::Permissions::ReadOnly,
                backend::FullReadChannelDiscipline::Close,
            ),
        };

        // If this client is to be the read-write client, and there's already
        // such a client, the old client needs to be evicted and stopped before
        // the new client can start. Create and install the new task under the
        // lock, then drop the lock before running down the old task.
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

        // Don't let the new task start until any old tasks have been run down.
        // This prevents writes from an old R/W task from being interleaved with
        // writes from a new one.
        task_start_tx
            .send(())
            .expect("new serial task shouldn't exit before starting");
    }

    /// Notifies all active clients that the instance is migrating to a new
    /// host.
    ///
    /// This function reads state from the console backend and relays it to
    /// clients. The caller should ensure that the VM is paused before calling
    /// this function so that backend's state doesn't change after this function
    /// captures it.
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
    backend_hdl: backend::Client,

    /// Receives output bytes from the guest.
    console_rx: mpsc::Receiver<u8>,

    /// Receives control commands that should be relayed to websocket clients
    /// (e.g. impending migrations).
    control_rx: mpsc::Receiver<InstanceSerialConsoleControlMessage>,

    /// Signaled to direct a task to enter its processing loop.
    start_rx: oneshot::Receiver<()>,

    /// Signaled to tell a task to shut down.
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

    // Wait to be told to start the main loop. This allows the task creation
    // logic to ensure that there's only one active console writer at a time.
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

                // In general it's OK to wait for this byte to be sent, since
                // read-write clients are allowed to block the backend and
                // read-only clients will be disconnected if they don't process
                // bytes in a timely manner. That said, even read-write clients
                // need to monitor `done_rx` here; otherwise a badly-behaved
                // client can prevent a VM from stopping.
                select! {
                    biased;

                    _ = &mut done_rx => {
                        probes::serial_event_done!(|| ());
                        close_reason = Some("VM stopped");
                        break;
                    }

                    _ = sink.send(Message::binary(vec![b])) => {}
                }
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
                // As above, don't let a client that's not processing control
                // messages prevent the VM from stopping.
                select! {
                    biased;

                    _ = &mut done_rx => {
                        probes::serial_event_done!(|| ());
                        close_reason = Some("VM stopped");
                        break;
                    }

                    _ = sink
                        .send(Message::Text(
                            serde_json::to_string(&control).expect(
                                "control messages can always serialize into JSON",
                            ),
                    )) => {}
                }
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
    let mut ws = sink.reunite(stream).expect("sink and stream should match");
    if let Err(e) = ws
        .close(Some(CloseFrame {
            code: CloseCode::Away,
            reason: close_reason
                .unwrap_or("serial connection task exited")
                .into(),
        }))
        .await
    {
        warn!(
            log, "error sending close frame to client";
            "client_id" => client_id.0,
            "error" => ?e,
        );
    }

    client_tasks.lock().unwrap().remove_by_id(client_id);
}
