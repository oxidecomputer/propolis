use std::collections::VecDeque;
use std::fs;
use std::io::{ErrorKind, Read, Result, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, Weak};

use crate::dispatch::events::{Event, EventTarget, FdEvents, Resource, Token};
use crate::dispatch::DispCtx;
use crate::util::self_arc::*;

use super::{Sink, Source};

pub struct UDSock {
    socks: Mutex<Socks>,
    sink_driver: Mutex<SinkDriver>,
    source_driver: Mutex<SourceDriver>,
    cv: Condvar,
    sa_cell: SelfArcCell<Self>,
}
struct Socks {
    state: SockState,
    server: UnixListener,
    client: Option<UnixStream>,
    client_token_fd: Option<Token>,
}
struct SinkDriver {
    sink: Option<Arc<dyn Sink>>,
    buf: VecDeque<u8>,
}
struct SourceDriver {
    source: Option<Arc<dyn Source>>,
    buf: VecDeque<u8>,
}

impl SinkDriver {
    fn new(bufsz: usize) -> Self {
        assert!(bufsz > 0);
        Self { sink: None, buf: VecDeque::with_capacity(bufsz) }
    }
}
impl SourceDriver {
    fn new(bufsz: usize) -> Self {
        assert!(bufsz > 0);
        Self { source: None, buf: VecDeque::with_capacity(bufsz) }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum BufState {
    Steady,
    ProcessCapable,
    ProcessRequired,
}

trait BufDriver {
    fn drive(&mut self);
    fn buffer_state(&self) -> BufState;

    fn drive_transfer(&mut self) -> Option<BufState> {
        let old = self.buffer_state();
        self.drive();
        let new = self.buffer_state();

        if new != old {
            Some(new)
        } else {
            None
        }
    }
}

impl BufDriver for SinkDriver {
    fn drive(&mut self) {
        if let Some(sink) = self.sink.as_ref() {
            while let Some(b) = self.buf.pop_front() {
                if !sink.sink_write(b) {
                    self.buf.push_front(b);
                    break;
                }
            }
        }
    }
    fn buffer_state(&self) -> BufState {
        if self.buf.len() == self.buf.capacity() {
            BufState::Steady
        } else if !self.buf.is_empty() {
            BufState::ProcessCapable
        } else {
            BufState::ProcessRequired
        }
    }
}

impl BufDriver for SourceDriver {
    fn drive(&mut self) {
        if let Some(source) = self.source.as_ref() {
            while self.buf.len() < self.buf.capacity() {
                if let Some(b) = source.source_read() {
                    self.buf.push_back(b);
                } else {
                    break;
                }
            }
        }
    }
    fn buffer_state(&self) -> BufState {
        if self.buf.is_empty() {
            BufState::Steady
        } else if self.buf.len() != self.buf.capacity() {
            BufState::ProcessCapable
        } else {
            BufState::ProcessRequired
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum SockState {
    Init,
    Listen(Token),
    Connected,
    ClientGone,
}

impl SelfArc for UDSock {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}
impl UDSock {
    pub fn bind(path: &Path) -> Result<Arc<Self>> {
        let sock = match UnixListener::bind(path) {
            Ok(sock) => sock,
            Err(e) => {
                if e.kind() != ErrorKind::AddrInUse {
                    return Err(e);
                }
                // XXX just blindly do remove
                fs::remove_file(path)?;
                UnixListener::bind(path)?
            }
        };
        sock.set_nonblocking(true)?;
        let mut this = Arc::new(Self {
            socks: Mutex::new(Socks {
                state: SockState::Init,
                server: sock,
                client: None,
                client_token_fd: None,
            }),
            sink_driver: Mutex::new(SinkDriver::new(16)),
            source_driver: Mutex::new(SourceDriver::new(16)),
            cv: Condvar::new(),
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);
        Ok(this)
    }

    pub fn attach_sink(&self, sink: Arc<dyn Sink>) {
        let mut state = self.sink_driver.lock().unwrap();

        let cb_self = self.self_weak();
        sink.sink_set_notifier(Box::new(move |ctx| {
            if let Some(sock) = Weak::upgrade(&cb_self) {
                sock.notify_sink_ready(ctx);
            }
        }));

        assert!(state.sink.is_none());
        state.sink = Some(sink);
    }
    pub fn attach_source(&self, source: Arc<dyn Source>) {
        let mut state = self.source_driver.lock().unwrap();

        let cb_self = self.self_weak();
        source.source_set_notifier(Box::new(move |ctx| {
            if let Some(sock) = Weak::upgrade(&cb_self) {
                sock.notify_source_ready(ctx);
            }
        }));

        assert!(state.source.is_none());
        state.source = Some(source);
    }

    fn notify_sink_ready(&self, ctx: &DispCtx) {
        let mut sink = self.sink_driver.lock().unwrap();
        if let Some(_new_state) = sink.drive_transfer() {
            drop(sink);

            self.config_notifications(
                &mut self.socks.lock().unwrap(),
                &mut self.sink_driver.lock().unwrap(),
                &mut self.source_driver.lock().unwrap(),
                ctx,
            );
        }
    }
    fn notify_source_ready(&self, ctx: &DispCtx) {
        let mut source = self.source_driver.lock().unwrap();
        if let Some(_new_state) = source.drive_transfer() {
            drop(source);

            self.config_notifications(
                &mut self.socks.lock().unwrap(),
                &mut self.sink_driver.lock().unwrap(),
                &mut self.source_driver.lock().unwrap(),
                ctx,
            );
        }
    }

    pub fn wait_for_connect(&self) {
        let guard = self
            .cv
            .wait_while(self.socks.lock().unwrap(), |s| match s.state {
                SockState::Init => {
                    panic!("waiting for client on non-listening socket");
                }
                SockState::Connected => false,
                _ => true,
            })
            .unwrap();
        // inner mutex uneeded afterwards
        drop(guard);
    }

    fn do_listen(&self, socks: &mut Socks, ctx: &DispCtx) {
        match socks.state {
            SockState::Init | SockState::ClientGone => {
                let token = ctx.event.fd_register(
                    socks.server.as_raw_fd(),
                    FdEvents::POLLIN,
                    self.self_weak(),
                );
                socks.state = SockState::Listen(token);
            }
            _ => {
                panic!("bad socket state {:?}", socks.state);
            }
        }
    }

    pub fn listen(&self, ctx: &DispCtx) {
        let mut socks = self.socks.lock().unwrap();
        self.do_listen(&mut socks, ctx);
    }

    fn config_notifications(
        &self,
        socks: &mut Socks,
        sink: &mut SinkDriver,
        source: &mut SourceDriver,
        ctx: &DispCtx,
    ) {
        let (sink_state, source_state) =
            (sink.buffer_state(), source.buffer_state());
        match (sink_state, source_state) {
            (BufState::Steady, BufState::Steady) => {
                if let Some(cur_token) = socks.client_token_fd {
                    ctx.event.fd_deregister(cur_token);
                    socks.client_token_fd = None;
                }
            }
            (BufState::ProcessRequired, _) | (_, BufState::ProcessRequired) => {
                let mut events = FdEvents::empty();
                if sink_state != BufState::Steady {
                    events.insert(FdEvents::POLLIN);
                }
                if source_state != BufState::Steady {
                    events.insert(FdEvents::POLLOUT);
                }
                if let Some(cur_token) = socks.client_token_fd {
                    ctx.event.fd_reregister(cur_token, events);
                } else {
                    let client_fd = socks.client.as_ref().unwrap().as_raw_fd();
                    socks.client_token_fd = Some(ctx.event.fd_register(
                        client_fd,
                        events,
                        self.self_weak(),
                    ));
                }
            }
            (BufState::ProcessCapable, BufState::ProcessCapable) => {
                // TODO: polling
            }
            (_, _) => {}
        }
    }

    fn drive_device(&self, socks: &mut Socks, ctx: &DispCtx) {
        let mut sink = self.sink_driver.lock().unwrap();
        let mut source = self.source_driver.lock().unwrap();
        sink.drive();
        source.drive();
        self.config_notifications(socks, &mut sink, &mut source, ctx);
    }
    fn drive_client(
        &self,
        socks: &mut Socks,
        revents: FdEvents,
        ctx: &DispCtx,
    ) {
        let mut client = socks.client.as_ref().unwrap();
        let mut buf = [0u8];
        let mut close_client = false;

        if revents.contains(FdEvents::POLLIN) {
            let mut sink = self.sink_driver.lock().unwrap();
            while sink.buffer_state() != BufState::Steady {
                match client.read(&mut buf) {
                    Ok(0) => {
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(_e) => {
                        close_client = true;
                        break;
                    }
                    Ok(_n) => {
                        sink.buf.push_back(buf[0]);
                    }
                }
            }
            drop(sink);
        }
        if revents.contains(FdEvents::POLLOUT) && !close_client {
            let mut source = self.source_driver.lock().unwrap();
            while source.buffer_state() != BufState::Steady {
                buf[0] = source.buf.pop_front().unwrap();
                if match client.write(&buf) {
                    Ok(0) => true,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => true,
                    Err(_e) => {
                        close_client = true;
                        true
                    }
                    Ok(_n) => false,
                } {
                    // failed the write, put the data back
                    source.buf.push_front(buf[0]);
                    break;
                }
            }
        }
        if close_client {
            self.close_client(socks, ctx);
        }
    }

    fn close_client(&self, socks: &mut Socks, ctx: &DispCtx) {
        assert_eq!(socks.state, SockState::Connected);
        assert!(socks.client.is_some());
        if let Some(token) = socks.client_token_fd {
            ctx.event.fd_deregister(token);
        }
        socks.client = None;
        socks.state = SockState::ClientGone;
        self.do_listen(socks, ctx);
    }
}

impl EventTarget for UDSock {
    fn event_process(&self, ev: &Event, ctx: &DispCtx) {
        let mut socks = self.socks.lock().unwrap();

        match socks.state {
            SockState::Listen(listen_tok) => {
                match socks.server.accept() {
                    Ok((client, _addr)) => {
                        ctx.event.fd_deregister(listen_tok);
                        socks.client = Some(client);
                        socks.state = SockState::Connected;
                        self.config_notifications(
                            &mut socks,
                            &mut self.sink_driver.lock().unwrap(),
                            &mut self.source_driver.lock().unwrap(),
                            ctx,
                        );
                        self.cv.notify_all();
                    }
                    Err(_e) => {
                        // XXX better handling?  for now just take another lap
                    }
                }
            }
            SockState::Connected => match ev.res {
                Resource::Fd(cfd, revents) => {
                    assert_eq!(cfd, socks.client.as_ref().unwrap().as_raw_fd());
                    assert_eq!(ev.token, socks.client_token_fd.unwrap());
                    if revents.intersects(FdEvents::POLLHUP | FdEvents::POLLERR)
                    {
                        self.close_client(&mut socks, ctx);
                        return;
                    }
                    self.drive_client(&mut socks, revents, ctx);
                    self.drive_device(&mut socks, ctx);
                }
            },
            state => {
                panic!("unexpected event {:?} in {:?} state", ev, state);
            }
        }
    }
}
