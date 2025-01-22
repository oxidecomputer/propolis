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
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use backend::ConsoleBackend;
use dropshot::WebsocketConnectionRaw;
use futures::{SinkExt, StreamExt};
use propolis_api_types::InstanceSerialConsoleControlMessage;
use slab::Slab;
use slog::{info, warn};
use tokio::{
    io::{AsyncReadExt, ReadHalf, SimplexStream},
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
    fn serial_task_done() {}
    fn serial_task_loop(read_from_ws: bool, has_bytes_to_write: bool) {}
    fn serial_task_backend_read(len: usize) {}
    fn serial_task_backend_write(len: usize) {}
    fn serial_task_console_disconnect() {}
    fn serial_task_ws_recv(len: usize) {}
    fn serial_task_ws_error() {}
    fn serial_task_ws_disconnect() {}
    fn serial_buffer_size(n: usize) {}
}

/// The size, in bytes, of the intermediate buffers used to store bytes as they
/// move from the guest character device to the individual reader tasks.
const SERIAL_READ_BUFFER_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ClientId(usize);

/// Specifies whether a client should be a read-write or read-only client.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    ReadWrite,
    ReadOnly,
}

/// The possible events that can be sent to a serial task's control channel.
enum ControlEvent {
    /// The task has been registered into the task set and can start its main
    /// loop.
    Start(ClientId, backend::Client),
    /// Another part of the server has dispatched a console session control
    /// message to this task.
    Message(InstanceSerialConsoleControlMessage),
}

impl std::fmt::Debug for ControlEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(arg0, _arg1) => {
                f.debug_tuple("Start").field(arg0).finish()
            }
            Self::Message(arg0) => {
                f.debug_tuple("Message").field(arg0).finish()
            }
        }
    }
}

/// Tracks an individual client of this console and its associated tokio task.
struct ClientTask {
    /// The join handle for this client's task.
    hdl: JoinHandle<()>,

    /// Receives server-level commands and notifications about this connection.
    control_tx: mpsc::Sender<ControlEvent>,

    /// Triggered when the serial console manager shuts down.
    done_tx: oneshot::Sender<()>,
}

/// The collection of [`ClientTask`]s belonging to a console manager.
#[derive(Default)]
struct ClientTasks {
    /// The set of registered tasks.
    tasks: Slab<ClientTask>,

    /// The ID of the current read-write client, if there is one.
    rw_client_id: Option<ClientId>,
}

impl ClientTasks {
    fn add(&mut self, task: ClientTask) -> ClientId {
        ClientId(self.tasks.insert(task))
    }

    /// Ensures the task with ID `id` is removed from this task set.
    fn ensure_removed_by_id(&mut self, id: ClientId) {
        if self.tasks.contains(id.0) {
            self.tasks.remove(id.0);
            match self.rw_client_id {
                Some(existing_id) if existing_id == id => {
                    self.rw_client_id = None;
                }
                _ => {}
            }
        }
    }

    /// If there is a read-write client in the task collection, removes it (from
    /// both the read-write client position and the task table) and returns it.
    fn remove_rw_client(&mut self) -> Option<ClientTask> {
        if let Some(id) = self.rw_client_id.take() {
            Some(self.tasks.remove(id.0))
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

        let mut tasks: Vec<ClientTask> =
            tasks.into_iter().map(|e| e.1).collect();

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
        let (console_rx, console_tx) =
            tokio::io::simplex(SERIAL_READ_BUFFER_SIZE);
        let (control_tx, control_rx) = mpsc::channel(1);
        let (task_done_tx, task_done_rx) = oneshot::channel();
        let discipline = match kind {
            ClientKind::ReadWrite => backend::FullReadChannelDiscipline::Block,
            ClientKind::ReadOnly => backend::FullReadChannelDiscipline::Close,
        };

        let prev_rw_task;
        let new_id;
        {
            let mut client_tasks = self.client_tasks.lock().unwrap();

            // There can only ever be one read-write client at a time. If one
            // already exists, remove it from the task tracker. It will be
            // replaced with this registrant's ID before the lock is dropped.
            prev_rw_task = if kind == ClientKind::ReadWrite {
                client_tasks.remove_rw_client()
            } else {
                None
            };

            let ctx = SerialTaskContext {
                log: self.log.clone(),
                ws,
                console_rx,
                control_rx,
                done_rx: task_done_rx,
                client_tasks: self.client_tasks.clone(),
            };

            let task = ClientTask {
                hdl: tokio::spawn(serial_task(ctx)),
                control_tx: control_tx.clone(),
                done_tx: task_done_tx,
            };

            new_id = client_tasks.add(task);
            if kind == ClientKind::ReadWrite {
                assert!(client_tasks.rw_client_id.is_none());
                client_tasks.rw_client_id = Some(new_id);
            }
        };

        // Register with the backend to get a client handle to pass back to the
        // new client task.
        //
        // Note that if the read-full discipline is Close and the guest is very
        // busily writing data, it is possible for the backend to decide the
        // channel is unresponsive before the client task is ever told to enter
        // its main loop. While not ideal, this is legal because it is also
        // possible for the channel to have filled between the time the task
        // processed its "start" message and the time it actually began to poll
        // its read channel for data. In either case the client will be
        // disconnected immediately and not get to read anything.
        let backend_hdl = match kind {
            ClientKind::ReadWrite => {
                self.backend.attach_read_write_client(console_tx, discipline)
            }
            ClientKind::ReadOnly => {
                self.backend.attach_read_only_client(console_tx, discipline)
            }
        };

        // If this task displaced a previous read-write task, make sure that
        // task has exited before allowing the new one to enter its main loop
        // to avoid accidentally interleaving multiple tasks' writes.
        if let Some(task) = prev_rw_task {
            let _ = task.done_tx.send(());
            let _ = task.hdl.await;
        }

        let _ = control_tx.send(ControlEvent::Start(new_id, backend_hdl)).await;
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
                .iter()
                .map(|(_, client)| client.control_tx.clone())
                .collect()
        };

        for client in clients {
            let _ = client
                .send(ControlEvent::Message(
                    InstanceSerialConsoleControlMessage::Migrating {
                        destination,
                        from_start,
                    },
                ))
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

    /// Receives output bytes from the guest.
    console_rx: ReadHalf<SimplexStream>,

    /// Receives commands and notifications from the console manager.
    control_rx: mpsc::Receiver<ControlEvent>,

    /// Signaled to tell a task to shut down.
    done_rx: oneshot::Receiver<()>,

    /// A reference to the manager's client task map, used to deregister this
    /// task on disconnection.
    client_tasks: Arc<Mutex<ClientTasks>>,
}

async fn serial_task(
    SerialTaskContext {
        log,
        ws,
        mut console_rx,
        mut control_rx,
        mut done_rx,
        client_tasks,
    }: SerialTaskContext,
) {
    enum Event {
        Done,
        ReadFromBackend(usize),
        ConsoleDisconnected,
        WroteToBackend(Result<usize, std::io::Error>),
        Control(ControlEvent),
        WebsocketMessage(Message),
        WebsocketError(tokio_tungstenite::tungstenite::Error),
        WebsocketDisconnected,
    }

    let (client_id, mut backend_hdl) = match control_rx.recv().await {
        Some(ControlEvent::Start(id, hdl)) => (id, hdl),
        Some(e) => {
            panic!("serial task's first message should be Start but was {e:?}")
        }
        None => panic!("serial task's control channel closed unexpectedly"),
    };

    info!(
        log,
        "serial console task started";
        "client_id" => client_id.0,
    );

    let mut client_defunct = backend_hdl.get_defunct_rx();
    let mut read_from_guest = vec![0; SERIAL_READ_BUFFER_SIZE];
    let mut remaining_to_send = VecDeque::new();
    let (mut sink, mut stream) = ws.split();
    let mut close_reason: Option<&'static str> = None;
    loop {
        use futures::future::Either;

        // If this is a read-write client that has read some bytes from the
        // websocket peer, create a future that will attempt to write those
        // bytes to the console backend, and hold off on reading more data from
        // the peer until those bytes are sent.
        //
        // Otherwise, create an always-pending "write to backend" future and
        // accept another message from the websocket.
        //
        // Read from the websocket even if the client is read-only to avoid
        // putting backpressure on the remote peer (and possibly causing it to
        // hang). This has the added advantage that if the peer disconnects, the
        // read-from-peer future will yield an error, allowing this task to
        // clean itself up without having to wait for another occasion to send a
        // message to the peer.
        let (read_from_ws, backend_write_fut) =
            if backend_hdl.can_write() && !remaining_to_send.is_empty() {
                remaining_to_send.make_contiguous();
                (
                    false,
                    Either::Left(
                        backend_hdl.write(remaining_to_send.as_slices().0),
                    ),
                )
            } else {
                (true, Either::Right(futures::future::pending()))
            };

        let ws_fut = if read_from_ws {
            Either::Left(stream.next())
        } else {
            Either::Right(futures::future::pending())
        };

        probes::serial_task_loop!(|| (
            read_from_ws,
            !remaining_to_send.is_empty()
        ));

        let event = select! {
            // The priority of these branches is important:
            //
            // 1. If the console manager asks to stop this task, it should stop
            //    immediately without doing any more work (the VM is going
            //    away).
            // 2. Similarly, if the backend's read task marked this client as
            //    defunct, the task should exit right away.
            // 3. New bytes written by the guest need to be processed before any
            //    other requests: if a guest outputs a byte while a read-write
            //    client is attached, the relevant vCPU will be blocked until
            //    the client processes the byte.
            biased;

            _ = &mut done_rx => {
                Event::Done
            }

            _ = client_defunct.changed() => {
                Event::ConsoleDisconnected
            }

            res = console_rx.read(read_from_guest.as_mut_slice()) => {
                match res {
                    Ok(bytes_read) => Event::ReadFromBackend(bytes_read),
                    Err(_) => Event::ConsoleDisconnected,
                }
            }

            control = control_rx.recv() => {
                Event::Control(control.expect(
                    "serial control channel should outlive its task"
                ))
            }

            res = backend_write_fut => {
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
                probes::serial_task_done!(|| ());
                close_reason = Some("VM stopped");
                break;
            }
            Event::ReadFromBackend(len) => {
                probes::serial_task_backend_read!(|| (len));

                // In general it's OK to wait for this byte to be sent, since
                // read-write clients are allowed to block the backend and
                // read-only clients will be disconnected if they don't process
                // bytes in a timely manner. That said, even read-write clients
                // need to monitor `done_rx` here; otherwise a badly-behaved
                // remote peer can prevent a VM from stopping.
                let res = select! {
                    biased;

                    _ = &mut done_rx => {
                        probes::serial_task_done!(|| ());
                        close_reason = Some("VM stopped");
                        break;
                    }

                    res = sink.send(
                        Message::binary(read_from_guest[..len].to_vec())
                    ) => { res }
                };

                if let Err(e) = res {
                    info!(
                        log, "failed to write to a console client";
                        "client_id" => client_id.0,
                        "error" => ?e
                    );
                    close_reason = Some("sending bytes to the client failed");
                    break;
                }
            }
            Event::ConsoleDisconnected => {
                probes::serial_task_console_disconnect!(|| ());
                info!(
                    log, "console backend closed its client channel";
                    "client_id" => client_id.0
                );
                break;
            }
            Event::Control(control) => {
                let control = match control {
                    ControlEvent::Start(..) => {
                        panic!("received a start message after starting");
                    }
                    ControlEvent::Message(msg) => msg,
                };

                // As above, don't let a peer that's not processing control
                // messages prevent the VM from stopping.
                select! {
                    biased;

                    _ = &mut done_rx => {
                        probes::serial_task_done!(|| ());
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
                            "read-write console connection closed by backend"
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

                probes::serial_task_backend_write!(|| written);
                drop(remaining_to_send.drain(..written));
            }
            Event::WebsocketMessage(msg) => {
                assert!(
                    remaining_to_send.is_empty(),
                    "should only read from the socket when the buffer is empty"
                );
                if let Message::Binary(bytes) = msg {
                    probes::serial_task_ws_recv!(|| (bytes.len()));
                    remaining_to_send.extend(bytes.as_slice());
                }
            }
            Event::WebsocketError(e) => {
                probes::serial_task_ws_error!(|| ());
                warn!(
                    log, "serial console websocket error";
                    "client_id" => client_id.0,
                    "error" => ?e
                );
                break;
            }
            Event::WebsocketDisconnected => {
                probes::serial_task_ws_disconnect!(|| ());
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

    client_tasks.lock().unwrap().ensure_removed_by_id(client_id);
}
