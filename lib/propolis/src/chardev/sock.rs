// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io::{ErrorKind, Result};
use std::num::NonZeroUsize;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::chardev::{pollers, Sink, Source};

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
    abort: AtomicBool,
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
                // XXX just blindly do remove
                fs::remove_file(path)?;
                StdUnixListener::bind(path)?
            }
        };
        lsock.set_nonblocking(true)?;

        let this = Arc::new(Self {
            inner: Mutex::new(Inner { std_sock: Some(lsock), client: None }),
            cv: Condvar::new(),
            abort: AtomicBool::new(false),
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
    ) {
        self.sink_buf.attach(sink.as_ref());
        self.source_buf.attach(source.as_ref());

        let this = Arc::clone(self);
        let _task = tokio::spawn(async move {
            let _ = this.run(sink, source).await;
            todo!("get async task hdl");
        });
    }

    fn notify_connected(&self, addr: Option<SocketAddr>) {
        let mut inner = self.inner.lock().unwrap();
        inner.client = addr;
        self.cv.notify_all();
    }

    pub fn wait_for_connect(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        if inner.client.is_some() {
            return true;
        }
        let inner = self
            .cv
            .wait_while(inner, |i| {
                let abort = self.abort.load(Ordering::Relaxed);
                !abort && i.client.is_none()
            })
            .unwrap();
        inner.client.is_some()
    }

    #[cfg(test)]
    pub fn wait_for_disconnect(&self) {
        let inner = self.inner.lock().unwrap();
        if inner.client.is_none() {
            return;
        }
        let _inner = self.cv.wait_while(inner, |i| i.client.is_some());
    }

    pub fn shutdown(&self) {
        self.abort.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    pub async fn run(
        &self,
        sink: Arc<dyn Sink>,
        source: Arc<dyn Source>,
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

            tokio::select! {
                _sink_done = Self::run_sink(
                    sink.as_ref(),
                    &self.sink_buf,
                    readh,
                ) => {},
                _source_done = Self::run_source(
                    source.as_ref(),
                    &self.source_buf,
                    writeh,
                ) => {},
            };

            self.notify_connected(None);
        }
        Ok(())
    }
    async fn run_sink(
        sink: &dyn Sink,
        sink_buf: &pollers::SinkBuffer,
        mut readh: OwnedReadHalf,
    ) -> Result<()> {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            let num = readh.read(&mut buf).await?;
            if num == 0 {
                // If the client is gone, we're done here
                return Ok(());
            }
            sink_buf.write(&buf[..num], sink).await;
        }
    }
    async fn run_source(
        source: &dyn Source,
        source_buf: &pollers::SourceBuffer,
        mut writeh: OwnedWriteHalf,
    ) -> Result<()> {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            if let Some(n) = source_buf.read(&mut buf, source).await {
                writeh.write_all(&buf[..n]).await?;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::*;
    use crate::chardev;

    use tempfile::NamedTempFile;

    struct TestChardev {
        sink_notify: chardev::NotifierCell<dyn Sink>,
        source_notify: chardev::NotifierCell<dyn Source>,
    }
    impl TestChardev {
        fn new() -> Self {
            Self {
                sink_notify: chardev::NotifierCell::new(),
                source_notify: chardev::NotifierCell::new(),
            }
        }
    }

    impl chardev::Sink for TestChardev {
        fn write(&self, _data: u8) -> bool {
            // Accept all writes
            true
        }

        fn set_notifier(&self, f: Option<chardev::SinkNotifier>) {
            self.sink_notify.set(f);
        }
    }
    impl chardev::Source for TestChardev {
        fn read(&self) -> Option<u8> {
            None
        }

        fn discard(&self, count: usize) -> usize {
            count
        }

        fn set_autodiscard(&self, _active: bool) {}

        fn set_notifier(&self, f: Option<chardev::SourceNotifier>) {
            self.source_notify.set(f);
        }
    }

    async fn wait_connected(sock: &Arc<UDSock>) -> bool {
        let wsock = sock.clone();
        tokio::spawn(async move {
            tokio::task::block_in_place(|| wsock.wait_for_connect())
        })
        .await
        .expect("failed to join on wait_for_connect")
    }
    async fn wait_disconnected(sock: &Arc<UDSock>) {
        let wsock = sock.clone();
        let _ = tokio::spawn(async move {
            tokio::task::block_in_place(|| wsock.wait_for_disconnect())
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bail_on_shutdown_sock() {
        let tempf = NamedTempFile::new().expect("can create tempfile");
        let sockpath = tempf.into_temp_path();

        let testdev = Arc::new(TestChardev::new());

        std::fs::remove_file(&sockpath)
            .expect("can unlink tempfile prior to sock bind");
        let sock = UDSock::bind(&sockpath).expect("socket bind succeeds");

        sock.spawn(testdev.clone(), testdev.clone());

        // Make sure that a client can successfully connect and disconnect

        let csock = UnixStream::connect(&sockpath)
            .expect("can connect to chardev sock");
        assert!(wait_connected(&sock).await);
        drop(csock);

        tokio::time::timeout(Duration::from_secs(1), wait_disconnected(&sock))
            .await
            .expect("socket transitions to disconnected within arb. timeout");

        let csock = UnixStream::connect(&sockpath)
            .expect("can connect to chardev sock");
        assert!(wait_connected(&sock).await);
        drop(csock);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_wait_for_connect() {
        let tempf = NamedTempFile::new().expect("can create tempfile");
        let sockpath = tempf.into_temp_path();

        let testdev = Arc::new(TestChardev::new());

        std::fs::remove_file(&sockpath)
            .expect("can unlink tempfile prior to sock bind");
        let sock = UDSock::bind(&sockpath).expect("socket bind succeeds");

        sock.spawn(testdev.clone(), testdev.clone());

        // Spawn a task to wait for a connection
        let wait_sock = sock.clone();
        let wait_task =
            tokio::spawn(async move { wait_connected(&wait_sock).await });

        // Now let's try to have it abort waiting for a connection
        sock.shutdown();

        let connected = wait_task.await.expect("failed to join wait_task");
        assert!(!connected);
    }
}
