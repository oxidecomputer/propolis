use std::fs;
use std::io::{ErrorKind, Result};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::chardev::{Sink, Source};
use crate::dispatch::Dispatcher;

use tokio::net::unix::SocketAddr;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

#[derive(Default)]
struct SinkDriver {
    sink: Mutex<Option<Arc<dyn Sink>>>,
    notify: Notify,
}
impl SinkDriver {
    async fn drive(&self, client: &UnixStream) -> Result<()> {
        let sink = {
            let guard = self.sink.lock().unwrap();
            guard.as_ref().map(|s| Arc::clone(s))
        };
        if sink.is_none() {
            return Ok(());
        }
        let sink = sink.unwrap();
        loop {
            let mut buf = [0u8];
            loop {
                match client.try_read(&mut buf) {
                    Ok(0) => {
                        return Ok(());
                    }
                    Ok(_n) => {
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        let _ = client.readable().await?;
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            loop {
                match sink.write(buf[0]) {
                    true => {
                        break;
                    }
                    false => {
                        self.notify.notified().await;
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct SourceDriver {
    source: Mutex<Option<Arc<dyn Source>>>,
    notify: Notify,
}
impl SourceDriver {
    async fn drive(&self, client: &UnixStream) -> Result<()> {
        let source = {
            let guard = self.source.lock().unwrap();
            guard.as_ref().map(|s| Arc::clone(s))
        };
        if source.is_none() {
            return Ok(());
        }
        let source = source.unwrap();
        loop {
            let buf = match source.read() {
                Some(b) => b,
                None => {
                    self.notify.notified().await;
                    continue;
                }
            };
            loop {
                match client.try_write(&[buf]) {
                    Ok(_n) => {
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        let _ = client.writable().await?;
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        }
    }
}

struct Inner {
    std_sock: Option<StdUnixListener>,
    client: Option<SocketAddr>,
}

pub struct UDSock {
    inner: Mutex<Inner>,
    cv: Condvar,
    sink_driver: SinkDriver,
    source_driver: SourceDriver,
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
            sink_driver: SinkDriver::default(),
            source_driver: SourceDriver::default(),
        });

        Ok(this)
    }
    pub fn attach(
        self: &Arc<Self>,
        sink: Arc<dyn Sink>,
        source: Arc<dyn Source>,
    ) {
        let weak = Arc::downgrade(self);
        sink.set_notifier(Box::new(move |_| {
            if let Some(driver) = Weak::upgrade(&weak) {
                driver.notify_sink();
            }
        }));
        let mut guard = self.sink_driver.sink.lock().unwrap();
        assert!(guard.is_none());
        *guard = Some(sink);
        drop(guard);

        let weak = Arc::downgrade(self);
        source.set_notifier(Box::new(move |_| {
            if let Some(driver) = Weak::upgrade(&weak) {
                driver.notify_source();
            }
        }));
        let mut guard = self.source_driver.source.lock().unwrap();
        assert!(guard.is_none());
        *guard = Some(source);
        drop(guard);
    }
    pub fn spawn(self: &Arc<Self>, disp: &Dispatcher) {
        use crate::dispatch::AsyncCtx;

        let this = Arc::clone(self);
        disp.spawn_async(|_actx: AsyncCtx| {
            Box::pin(async move {
                let _ = this.run().await;
            })
        });
    }

    fn notify_sink(&self) {
        self.sink_driver.notify.notify_one();
    }
    fn notify_source(&self) {
        self.source_driver.notify.notify_one();
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

    pub async fn run(&self) -> Result<()> {
        let lsock = {
            let mut inner = self.inner.lock().unwrap();
            let sock = inner.std_sock.take().unwrap();
            drop(inner);
            sock
        };
        let lsock = UnixListener::from_std(lsock)?;
        while let Ok((sock, addr)) = lsock.accept().await {
            self.notify_connected(Some(addr));
            let _res = tokio::try_join!(
                self.sink_driver.drive(&sock),
                self.source_driver.drive(&sock),
            )?;
            self.notify_connected(None);
        }
        Ok(())
    }
}
