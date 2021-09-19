use std::fs;
use std::io::{ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::chardev::{pollers, Sink, Source};
use crate::dispatch::AsyncCtx;
use crate::dispatch::Dispatcher;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf, SocketAddr};
use tokio::net::UnixListener;

const BUF_SIZE: usize = 512;
const POLL_INTERVAL_MS: usize = 10;
const POLL_MISS_THRESH: usize = 5;

struct Inner {
    std_sock: Option<StdUnixListener>,
    client: Option<SocketAddr>,
}

pub struct UDSock {
    inner: Mutex<Inner>,
    cv: Condvar,
    sink_buf: Arc<pollers::SinkBuffer>,
    source_buf: Arc<pollers::SourceBuffer>,
}
impl UDSock {
    pub fn bind(path: &Path) -> Result<Arc<Self>> {
        let lsock = match StdUnixListener::bind(path) {
            Ok(sock) => sock,
            Err(e) => {
                if e.kind() != ErrorKind::AddrInUse {
                    return Err(e);
                }
                eprintln!(
                    "Couldn't bind to {}, removing and trying again...",
                    path.to_string_lossy()
                );
                // XXX just blindly do remove
                fs::remove_file(path)?;
                StdUnixListener::bind(path)?
            }
        };
        lsock.set_nonblocking(true)?;

        let this = Arc::new(Self {
            inner: Mutex::new(Inner { std_sock: Some(lsock), client: None }),
            cv: Condvar::new(),
            sink_buf: pollers::SinkBuffer::new(
                NonZeroUsize::new(BUF_SIZE).unwrap(),
            ),
            source_buf: pollers::SourceBuffer::new(pollers::Params {
                poll_interval: Duration::from_millis(POLL_INTERVAL_MS as u64),
                poll_miss_thresh: POLL_MISS_THRESH,
                buf_size: NonZeroUsize::new(BUF_SIZE).unwrap(),
            }),
        });

        Ok(this)
    }
    pub fn spawn(
        self: &Arc<Self>,
        sink: Arc<dyn Sink>,
        source: Arc<dyn Source>,
        disp: &Dispatcher,
    ) {
        self.sink_buf.attach(sink.as_ref());
        self.source_buf.attach(source.as_ref());

        let this = Arc::clone(self);
        disp.spawn_async(|mut actx: AsyncCtx| async move {
            let _ = this.run(sink, source, &mut actx).await;
        });
    }

    fn notify_connected(&self, addr: Option<SocketAddr>) {
        let mut inner = self.inner.lock().unwrap();
        inner.client = addr;
        self.cv.notify_all();
    }

    pub fn wait_for_connect(&self) {
        let inner = self.inner.lock().unwrap();
        if inner.client.is_some() {
            return;
        }
        let _inner = self.cv.wait_while(inner, |i| i.client.is_none());
    }

    pub async fn run(
        &self,
        sink: Arc<dyn Sink>,
        source: Arc<dyn Source>,
        actx: &AsyncCtx,
    ) -> Result<()> {
        let lsock = {
            let mut inner = self.inner.lock().unwrap();
            let sock = inner.std_sock.take().unwrap();
            drop(inner);
            sock
        };
        let lsock = UnixListener::from_std(lsock)?;
        while let Ok((sock, addr)) = lsock.accept().await {
            self.notify_connected(Some(addr));
            let (readh, writeh) = sock.into_split();

            let sink_buf = Arc::clone(&self.sink_buf);
            let sink_moved = Arc::clone(&sink);
            let sink_task = actx.spawn_async(|actx: AsyncCtx| async move {
                let _ =
                    Self::run_sink(sink_moved, sink_buf, readh, &actx).await;
            });

            let source_buf = Arc::clone(&self.source_buf);
            let source_moved = Arc::clone(&source);
            let source_task = actx.spawn_async(|actx: AsyncCtx| async move {
                let _ =
                    Self::run_source(source_moved, source_buf, writeh, &actx)
                        .await;
            });

            tokio::select! {
                _ = actx.wait_exited(sink_task) => {
                    actx.cancel_async(source_task);
                },
                _ = actx.wait_exited(source_task) => {
                    actx.cancel_async(sink_task);
                },
            };

            self.notify_connected(None);
        }
        Ok(())
    }
    async fn run_sink(
        sink: Arc<dyn Sink>,
        sink_buf: Arc<pollers::SinkBuffer>,
        mut readh: OwnedReadHalf,
        actx: &AsyncCtx,
    ) -> Result<()> {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            let num = readh.read(&mut buf).await?;
            sink_buf.write(&buf[..num], sink.as_ref(), actx).await;
        }
    }
    async fn run_source(
        source: Arc<dyn Source>,
        source_buf: Arc<pollers::SourceBuffer>,
        mut writeh: OwnedWriteHalf,
        actx: &AsyncCtx,
    ) -> Result<()> {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            if let Some(n) =
                source_buf.read(&mut buf, source.as_ref(), actx).await
            {
                writeh.write(&buf[..n]).await?;
            }
        }
    }
}
