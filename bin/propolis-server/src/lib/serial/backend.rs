// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A backend that provides external clients with access to a Propolis character
//! device and tracks the history of bytes the guest has written to that device.
//!
//! The [`ConsoleBackend`] type provides an interface to read and write a
//! character device. The backend itself holds a reference to the device and
//! manages a task that reads bytes from the device and issues them to any
//! registered readers.
//!
//! Users of the backend call [`ConsoleBackend::attach_read_only_client`] or
//! [`ConsoleBackend::attach_read_write_client`] to obtain a [`Client`]
//! representing the new connection. Each client has a corresponding [`Reader`]
//! owned by the backend's [`read_task`]; each `Reader` contains a tokio
//! [`SimplexStream`] to which the read task sends bytes written by the guest.
//! Read-write clients own a reference to a [`Writer`] that can write bytes to a
//! sink associated with the backend's character device.
//!
//! Clients may be disconnected in one of two ways:
//!
//! - A client's owner can just drop its `Client` struct.
//! - If the client was configured to use the "close on full channel" read
//!   discipline, and the read task is unable to send some bytes to a reader's
//!   stream because the channel is full, the client is invalidated.
//!
//! To avoid circular references, clients and their readers don't refer to each
//! other directly. Instead, they share both sides of a `tokio::watch` to which
//! they publish `true` when a connection has been invalidated, regardless of
//! who invalidated it. The receiver end of this channel is available to users
//! through [`Client::get_defunct_rx`] to allow clients' users to learn when
//! their clients have been closed.

use std::{
    future::Future,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use propolis::chardev::{
    pollers::{SinkBuffer, SourceBuffer},
    Sink, Source,
};
use tokio::{
    io::{AsyncWriteExt, SimplexStream, WriteHalf},
    select,
    sync::{mpsc, oneshot, watch},
};

use super::history_buffer::{HistoryBuffer, SerialHistoryOffset};

/// A client's rights when accessing a character backend.
#[derive(Clone, Copy)]
enum Permissions {
    /// The client may both read and write to the backend.
    ReadWrite,

    /// The client may only read from the backend.
    ReadOnly,
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

/// An entity that receives bytes written by the guest.
///
/// Readers are instantiated in [`ConsoleBackend::attach_client`] and passed via
/// channel to the backend's [`read_task`].
struct Reader {
    /// Bytes received from the guest are sent into this stream.
    tx: WriteHalf<SimplexStream>,

    /// Determines what happens if a read fails to accept all of the bytes the
    /// task wanted to write.
    discipline: FullReadChannelDiscipline,

    /// Set to `true` when this reader is dropped.
    defunct_tx: watch::Sender<bool>,

    /// Set to `true` when this reader's associated [`Client`] is dropped.
    defunct_rx: watch::Receiver<bool>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.defunct_tx.send(true);
    }
}

/// An entity that can attempt to write bytes to the guest.
struct Writer {
    /// The backend to which this writer directs its writes.
    backend: Arc<ConsoleBackend>,
}

impl Writer {
    /// Sends the bytes in `buf` to the sink associated with this writer's
    /// backend.
    async fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        assert!(!buf.is_empty());

        Ok(self
            .backend
            .sink_buffer
            .write(buf, self.backend.sink.as_ref())
            .await
            .expect("chardev sink writes always succeed"))
    }
}

/// A client of this backend.
pub(super) struct Client {
    /// The client's associated [`Writer`], if it was instantiated as a
    /// read-write client.
    writer: Option<Writer>,

    /// The client writes `true` to this channel when it is dropped.
    defunct_tx: watch::Sender<bool>,

    /// The read task writes `true` to this channel if this client's associated
    /// [`Reader`] is closed.
    defunct_rx: watch::Receiver<bool>,
}

impl Client {
    pub(super) fn can_write(&self) -> bool {
        self.writer.is_some()
    }

    pub(super) fn get_defunct_rx(&self) -> watch::Receiver<bool> {
        self.defunct_rx.clone()
    }

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
    /// - `Err(std::io::Error::PermissionDenied)` if this client does not have
    ///   write access to the device.
    /// - `Err(std::io::Error::ConnectionAborted)` if this client lost its
    ///   ability to write to the device because it stopped servicing reads,
    ///   i.e., its read channel was full and it uses the close-on-full-channel
    ///   read discipline.
    ///
    /// # Cancel safety
    ///
    /// The future returned by this function is cancel-safe. If it is dropped,
    /// it is guaranteed that no bytes were written to the backend or its
    /// device.
    pub(super) async fn write(
        &mut self,
        buf: &[u8],
    ) -> Result<usize, std::io::Error> {
        if buf.is_empty() {
            return Ok(0);
        }

        let Some(writer) = self.writer.as_mut() else {
            return Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ));
        };

        if *self.defunct_rx.borrow_and_update() {
            return Err(std::io::Error::from(
                std::io::ErrorKind::ConnectionAborted,
            ));
        }

        writer.write(buf).await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.defunct_tx.send(true);
    }
}

/// Character backend state that's protected by a lock.
struct Inner {
    /// A history of the bytes read from this backend's device.
    buffer: HistoryBuffer,
}

impl Inner {
    fn new(history_size: usize) -> Self {
        Self { buffer: HistoryBuffer::new(history_size) }
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

    /// Receives the [`Reader`]s generated when new clients connect to this
    /// backend.
    reader_tx: mpsc::UnboundedSender<Reader>,

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

        let (reader_tx, reader_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();

        let this = Arc::new(Self {
            inner: Arc::new(Mutex::new(Inner::new(history_bytes))),
            sink,
            sink_buffer,
            reader_tx,
            done_tx,
        });

        let inner = this.inner.clone();
        tokio::spawn(read_task(inner, source, reader_rx, done_rx));

        this
    }

    /// Attaches a new read-only client to this backend. Incoming bytes from the
    /// guest are sent to `read_tx`. `discipline` specifies what happens if the
    /// channel is full when new bytes are available to read.
    ///
    /// The caller may disconnect its session by dropping the returned
    /// [`Client`].
    pub(super) fn attach_read_only_client(
        self: &Arc<Self>,
        read_tx: WriteHalf<SimplexStream>,
        discipline: FullReadChannelDiscipline,
    ) -> Client {
        self.attach_client(read_tx, Permissions::ReadOnly, discipline)
    }

    /// Attaches a new read-write client to this backend. Incoming bytes from
    /// the guest are sent to `read_tx`. `discipline` specifies what happens if
    /// the channel is full when new bytes are available to read.
    ///
    /// The caller may disconnect its session by dropping the returned
    /// [`Client`].
    pub(super) fn attach_read_write_client(
        self: &Arc<Self>,
        read_tx: WriteHalf<SimplexStream>,
        discipline: FullReadChannelDiscipline,
    ) -> Client {
        self.attach_client(read_tx, Permissions::ReadWrite, discipline)
    }

    /// Attaches a new client to this backend, returning a [`Client`] that
    /// represents the caller's connection to the backend.
    fn attach_client(
        self: &Arc<Self>,
        read_tx: WriteHalf<SimplexStream>,
        permissions: Permissions,
        discipline: FullReadChannelDiscipline,
    ) -> Client {
        let (defunct_tx, defunct_rx) = watch::channel(false);
        let writer = match permissions {
            Permissions::ReadWrite => Some(Writer { backend: self.clone() }),
            Permissions::ReadOnly => None,
        };

        let reader = Reader {
            tx: read_tx,
            discipline,
            defunct_tx: defunct_tx.clone(),
            defunct_rx: defunct_rx.clone(),
        };

        // Unwrapping is safe here because `read_task` is only allowed to exit
        // after the backend signals `done_tx`, which only happens when the
        // backend is dropped.
        self.reader_tx.send(reader).unwrap();

        Client { writer, defunct_tx, defunct_rx }
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
    mut reader_rx: mpsc::UnboundedReceiver<Reader>,
    mut done_rx: oneshot::Receiver<()>,
) {
    let buf = SourceBuffer::new(propolis::chardev::pollers::Params {
        poll_interval: std::time::Duration::from_millis(10),
        poll_miss_thresh: 5,
        buf_size: NonZeroUsize::new(super::SERIAL_READ_BUFFER_SIZE).unwrap(),
    });
    buf.attach(source.as_ref());

    enum Event {
        NewReader(Reader),
        BytesRead(usize),
    }

    let mut readers = vec![];
    let mut bytes = vec![0; super::SERIAL_READ_BUFFER_SIZE];
    loop {
        let event = select! {
            biased;

            _ = &mut done_rx => {
                return;
            }

            new_reader = reader_rx.recv() => {
                let Some(reader) = new_reader else {
                    return;
                };

                Event::NewReader(reader)
            }

            bytes_read = buf.read(bytes.as_mut_slice(), source.as_ref()) => {
                Event::BytesRead(
                    bytes_read.expect("SourceBuffer reads are infallible")
                )
            }
        };

        // Returns `true` if it was possible to send the entirety of `to_send`
        // to `reader` without violating the reader's full-channel discipline.
        async fn send_ok(reader: &mut Reader, to_send: &[u8]) -> bool {
            match reader.discipline {
                // Reads and writes to simplex streams (unlike channels) do not
                // resolve to errors if the other half of the stream is dropped
                // while the read or write is outstanding; instead, the future
                // stays pending forever.
                //
                // To handle cases where the reader's client handle was dropped
                // mid-read, select over the attempt to write and the "defunct"
                // watcher and retire the client if the watcher fires first.
                FullReadChannelDiscipline::Block => {
                    select! {
                        res = reader.tx.write_all(to_send) => {
                            res.is_ok()
                        }

                        _ = reader.defunct_rx.changed() => {
                            false
                        }
                    }
                }
                // In the close-on-full-channel case it suffices to poll the
                // future exactly once and see if this manages to write all of
                // the data. No selection is needed here: if `write` returns
                // `Poll::Pending`, `poll_once` will return an error,
                // irrespective of the reason the write didn't complete.
                FullReadChannelDiscipline::Close => {
                    matches!(
                        poll_once(reader.tx.write(to_send)),
                        Some(Ok(len)) if len == to_send.len()
                    )
                }
            }
        }

        match event {
            Event::NewReader(r) => readers.push(r),
            Event::BytesRead(bytes_read) => {
                let to_send = &bytes[0..bytes_read];

                // Send the bytes to each reader, dropping readers who fail to
                // accept all the bytes on offer.
                //
                // It would be nice to use `Vec::retain_mut` here, but async
                // closures aren't quite stable yet, so hand-roll a while loop
                // instead.
                let mut idx = 0;
                while idx < readers.len() {
                    let ok = send_ok(&mut readers[idx], to_send).await;
                    if ok {
                        idx += 1;
                    } else {
                        readers.swap_remove(idx);
                    }
                }

                inner.lock().unwrap().buffer.consume(to_send);
            }
        }
    }
}

/// A helper function to poll a future `f` exactly once, returning its output
/// `Some(R)` if it is immediately ready and `None` otherwise.
fn poll_once<R>(f: impl Future<Output = R>) -> Option<R> {
    tokio::pin!(f);

    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(result) => Some(result),
        Poll::Pending => None,
    }
}
