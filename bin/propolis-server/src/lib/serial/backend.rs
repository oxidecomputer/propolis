// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A backend that provides external clients with access to a Propolis character
//! device and tracks the history of bytes the guest has written to that device.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use propolis::chardev::{
    pollers::{SinkBuffer, SourceBuffer},
    Sink, Source,
};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};

use super::history_buffer::{HistoryBuffer, SerialHistoryOffset};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ClientId(u64);

/// A client's rights when accessing a character backend.
#[derive(Clone, Copy)]
pub(super) enum Permissions {
    /// The client may both read and write to the backend.
    ReadWrite,

    /// The client may only read from the backend.
    ReadOnly,
}

impl Permissions {
    pub(super) fn is_writable(&self) -> bool {
        matches!(self, Permissions::ReadWrite)
    }
}

/// Determines what happens when the backend is unable to send a guest byte back
/// to a client because the client's channel is full.
#[derive(Clone, Copy)]
pub(super) enum FullReadChannelDiscipline {
    /// The backend should block until it can send to this client.
    Block,

    /// The backend should close this client's connection.
    Close,
}

/// An individual client of a character backend.
struct ClientState {
    /// Bytes read from the character device should be sent to the client on
    /// this channel.
    tx: mpsc::Sender<u8>,

    /// Determines what happens when the backend wants to send a byte and
    /// [`Self::tx`] is full.
    read_discipline: FullReadChannelDiscipline,
}

/// A handle held by a client that represents its connection to the backend.
pub(super) struct Client {
    /// The client's ID.
    id: ClientId,

    /// A reference to the backend to which the client is connected.
    backend: Arc<ConsoleBackend>,

    /// The client's backend access rights.
    permissions: Permissions,
}

impl Client {
    pub(super) fn is_writable(&self) -> bool {
        self.permissions.is_writable()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut inner = self.backend.inner.lock().unwrap();
        inner.clients.remove(&self.id);
    }
}

impl Client {
    /// Attempts to write the bytes in `buf` to the backend.
    ///
    /// The backend may buffer some of the written bytes before sending them to
    /// its device, so when this function returns, it is not guaranteed that all
    /// of the written bytes have actually reached the guest.
    ///
    /// # Return value
    ///
    /// - `Ok(bytes)` if the write succeeded; `bytes` is the number of bytes
    ///   that were written.
    /// - `Err(ErrorKind::PermissionDenied)` if this handle does not grant write
    ///   access to the device.
    /// - `Err(ErrorKind::ConnectionAborted)` if this handle's client was
    ///   previously disconnected from the backend, e.g. because it had a full
    ///   read channel and was using the close-on-full-channel read discipline.
    ///
    /// # Cancel safety
    ///
    /// The future returned by this function is cancel-safe. If it is dropped,
    /// it is guaranteed that no bytes were written to the backend or its
    /// device. See [`SinkBuffer::write`].
    pub(super) async fn write(
        &mut self,
        buf: &[u8],
    ) -> Result<usize, std::io::Error> {
        if buf.is_empty() {
            return Ok(0);
        }

        if !matches!(self.permissions, Permissions::ReadWrite) {
            return Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ));
        }

        // The client handle may have been invalidated if it used the "close on
        // full channel" discipline. If this client is no longer active, return
        // an error.
        //
        // The read dispatcher task doesn't synchronize with writes, so it's
        // possible for the client to be invalidated after the lock is dropped.
        // (It can't be held while writing to the backend because
        // `SinkBuffer::write` is async.) This is OK; since reads and writes
        // aren't synchronized to begin with, in any situation like this it was
        // equally possible for the write to finish before the client was
        // invalidated.
        {
            let inner = self.backend.inner.lock().unwrap();
            if !inner.clients.contains_key(&self.id) {
                return Err(std::io::Error::from(
                    std::io::ErrorKind::ConnectionAborted,
                ));
            }
        }

        Ok(self
            .backend
            .sink_buffer
            .write(buf, self.backend.sink.as_ref())
            .await
            .expect("chardev sink writes always succeed"))
    }
}

/// Character backend state that's protected by a lock.
struct Inner {
    /// A history of the bytes read from this backend's device.
    buffer: HistoryBuffer,

    /// A table mapping client IDs to clients.
    clients: BTreeMap<ClientId, ClientState>,

    /// The ID to assign to the next client to attach to this backend.
    next_client_id: u64,
}

impl Inner {
    fn new(history_size: usize) -> Self {
        Self {
            buffer: HistoryBuffer::new(history_size),
            clients: BTreeMap::new(),
            next_client_id: 0,
        }
    }

    fn next_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id += 1;
        ClientId(id)
    }
}

/// A backend for a Propolis serial console that allows multiple clients to
/// access a single serial device.
pub struct ConsoleBackend {
    inner: Arc<Mutex<Inner>>,

    /// The character [`Sink`] that should receive writes to this backend.
    /// Writes should not access the sink directly; instead, they should be
    /// directed to [`Self::sink_buffer`]. This reference is needed because
    /// [`SinkBuffer`]'s current API requires a sink to be passed into each
    /// attempt to write (buffers don't own references to their sinks).
    sink: Arc<dyn Sink>,

    /// The buffer that sits in front of this backend's sink. Writes to the
    /// backend should be directed at the buffer, not at [`Self::sink`].
    sink_buffer: Arc<SinkBuffer>,

    /// A channel used to tell the backend's reader task that the backend has
    /// been closed.
    done_tx: oneshot::Sender<()>,
}

impl ConsoleBackend {
    pub fn new<T: Source + Sink>(
        device: Arc<T>,
        history_bytes: usize,
    ) -> Arc<Self> {
        const SINK_BUFFER_BYTES: usize = 64;

        let sink_buffer =
            SinkBuffer::new(NonZeroUsize::new(SINK_BUFFER_BYTES).unwrap());
        sink_buffer.attach(device.as_ref());

        let sink = device.clone();
        let source = device.clone();
        let (done_tx, done_rx) = oneshot::channel();

        let this = Arc::new(Self {
            inner: Arc::new(Mutex::new(Inner::new(history_bytes))),
            sink,
            sink_buffer,
            done_tx,
        });

        let inner = this.inner.clone();
        tokio::spawn(async move {
            read_task(inner, source, done_rx).await;
        });

        this
    }

    /// Attaches a new client to this backend, yielding a handle that the client
    /// can use to issue further operations.
    ///
    /// # Arguments
    ///
    /// - `read_tx`: A channel to which the backend should send bytes read from
    ///   its device.
    /// - `permissions`: The read/write permissions this client should have.
    /// - `full_read_tx_discipline`: Describes what should happen if the reader
    ///   task ever finds that `read_tx` is full when dispatching a byte to it.
    pub(super) fn attach_client(
        self: &Arc<Self>,
        read_tx: mpsc::Sender<u8>,
        permissions: Permissions,
        full_read_tx_discipline: FullReadChannelDiscipline,
    ) -> Client {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_client_id();
        let client = ClientState {
            tx: read_tx,
            read_discipline: full_read_tx_discipline,
        };

        inner.clients.insert(id, client);
        Client { id, backend: self.clone(), permissions }
    }

    /// Returns the contents of this backend's history buffer. See
    /// [`HistoryBuffer::contents_vec`].
    pub fn history_vec(
        &self,
        byte_offset: SerialHistoryOffset,
        max_bytes: Option<usize>,
    ) -> Result<(Vec<u8>, usize), super::history_buffer::Error> {
        let inner = self.inner.lock().unwrap();
        inner.buffer.contents_vec(byte_offset, max_bytes)
    }

    /// Returns the number of bytes that have ever been sent to this backend's
    /// history buffer.
    pub fn bytes_since_start(&self) -> usize {
        self.inner.lock().unwrap().buffer.bytes_from_start()
    }
}

impl Drop for ConsoleBackend {
    fn drop(&mut self) {
        let (tx, _rx) = oneshot::channel();
        let done_tx = std::mem::replace(&mut self.done_tx, tx);
        let _ = done_tx.send(());
    }
}

mod migrate {
    use propolis::migrate::{
        MigrateCtx, MigrateSingle, MigrateStateError, PayloadOffer,
    };

    use crate::serial::history_buffer::migrate::HistoryBufferContentsV1;

    use super::ConsoleBackend;

    impl MigrateSingle for ConsoleBackend {
        fn export(
            &self,
            _ctx: &MigrateCtx,
        ) -> Result<propolis::migrate::PayloadOutput, MigrateStateError>
        {
            Ok(self.inner.lock().unwrap().buffer.export().into())
        }

        fn import(
            &self,
            mut offer: PayloadOffer,
            _ctx: &MigrateCtx,
        ) -> Result<(), MigrateStateError> {
            let contents: HistoryBufferContentsV1 = offer.parse()?;
            self.inner.lock().unwrap().buffer.import(contents);
            Ok(())
        }
    }
}

/// Reads bytes from the supplied `source` and dispatches them to the clients in
/// `inner`. Each backend is expected to spin up one task that runs this
/// function.
async fn read_task(
    inner: Arc<Mutex<Inner>>,
    source: Arc<dyn Source>,
    mut done_rx: oneshot::Receiver<()>,
) {
    const READ_BUFFER_SIZE_BYTES: usize = 512;
    let buf = SourceBuffer::new(propolis::chardev::pollers::Params {
        poll_interval: std::time::Duration::from_millis(10),
        poll_miss_thresh: 5,
        buf_size: NonZeroUsize::new(READ_BUFFER_SIZE_BYTES).unwrap(),
    });
    buf.attach(source.as_ref());

    let mut bytes = vec![0; READ_BUFFER_SIZE_BYTES];
    loop {
        let bytes_read = select! {
            biased;

            _ = &mut done_rx => {
                return;
            }

            res = buf.read(bytes.as_mut_slice(), source.as_ref()) => {
                res.unwrap()
            }
        };

        let to_send = &bytes[0..bytes_read];

        // Capture a list of all the clients who should receive this byte with
        // the lock held, then drop the lock before sending to any of them. Note
        // that sends to clients may block for an arbitrary length of time: the
        // receiver may be relaying received bytes to a websocket, and the
        // remote peer may be slow to accept them.
        struct CapturedClient {
            id: ClientId,
            tx: mpsc::Sender<u8>,
            discipline: FullReadChannelDiscipline,
            disconnect: bool,
        }

        let mut clients = {
            let guard = inner.lock().unwrap();
            guard
                .clients
                .iter()
                .map(|(id, client)| CapturedClient {
                    id: *id,
                    tx: client.tx.clone(),
                    discipline: client.read_discipline,
                    disconnect: false,
                })
                .collect::<Vec<CapturedClient>>()
        };

        // Prepare to delete any clients that are no longer active (i.e., who
        // have dropped the receiver sides of their channels) or who are using
        // the close-on-full-channel discipline and who have a full channel.
        for byte in to_send {
            for client in clients.iter_mut() {
                client.disconnect = match client.discipline {
                    FullReadChannelDiscipline::Block => {
                        client.tx.send(*byte).await.is_err()
                    }
                    FullReadChannelDiscipline::Close => {
                        client.tx.try_send(*byte).is_err()
                    }
                }
            }
        }

        // Clean up any clients who met the disconnection criteria.
        let mut guard = inner.lock().unwrap();
        guard.buffer.consume(to_send);
        for client in clients.iter().filter(|c| c.disconnect) {
            guard.clients.remove(&client.id);
        }
    }
}
