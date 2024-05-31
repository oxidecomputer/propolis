// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2024 Oxide Computer Company

//! Utilities for using rfb over a tungstnite websocket

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

use futures::{sink::Sink, stream::Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::error::Error as TungError;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

/// Convert from Tungstenite error to io::Error
fn tung_err_to_io(err: TungError) -> io::Error {
    match err {
        TungError::Io(io_err) => io_err,
        err => io::Error::new(io::ErrorKind::Other, err),
    }
}

/// Wrap a [WebSocketStream] so it implements [AsyncRead] and [AsyncWrite]
pub struct BinaryWs<T> {
    ws: WebSocketStream<T>,
    buf: Option<(Vec<u8>, usize)>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> BinaryWs<T> {
    pub fn new(ws: WebSocketStream<T>) -> Self {
        Self { ws, buf: None }
    }
}
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for BinaryWs<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let ws = Pin::new(&mut self.ws);
        match ws.poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                let ws = Pin::new(&mut self.ws);
                let msg = Message::binary(buf);
                if let Err(e) = ws.start_send(msg) {
                    Poll::Ready(Err(tung_err_to_io(e)))
                } else {
                    Poll::Ready(Ok(buf.len()))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(tung_err_to_io(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let ws = Pin::new(&mut self.ws);
        ws.poll_flush(cx).map_err(tung_err_to_io)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let ws = Pin::new(&mut self.ws);
        ws.poll_close(cx).map_err(tung_err_to_io)
    }
}
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for BinaryWs<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            // Emit cached data which has not been read yet
            if let Some((msg, consumed)) = self.buf.take() {
                let (_used, remain) = msg.split_at(consumed);
                let to_write = buf.remaining().min(remain.len());
                buf.put_slice(&remain[..to_write]);
                if to_write < remain.len() {
                    self.buf = Some((msg, consumed + to_write))
                }
                return Poll::Ready(Ok(()));
            }

            // Otherwise poll for more data to receive
            let ws = Pin::new(&mut self.ws);
            match ws.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(e))) => match e {
                    tokio_tungstenite::tungstenite::Error::Io(ioe) => {
                        return Poll::Ready(Err(ioe));
                    }
                    _ => {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            e,
                        )));
                    }
                },
                Poll::Ready(Some(Ok(rmsg))) => {
                    if let Message::Binary(msgbuf) = rmsg {
                        self.buf = Some((msgbuf, 0));
                        continue;
                    }
                    // For all other types, ignore and continue polling
                }
            }
        }
    }
}
