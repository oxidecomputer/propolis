use std::fs;
use std::io::{ErrorKind, Read, Result, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, Weak};

use super::base::Uart;
use crate::common::*;
use crate::dispatch::event_disp::{EventTargetFd, SubId};
use crate::dispatch::DispCtx;
use crate::intr_pins::{IntrPin, LegacyPin};
use crate::pio::PioDev;
use crate::util::self_arc::*;

// XXX: raw event usage for now
use libc::POLLIN;

pub const REGISTER_LEN: usize = 8;

pub struct LpcUart {
    state: Mutex<UartState>,
    sock: Arc<UartSock>,
    sa_cell: SelfArcCell<Self>,
}
struct UartState {
    uart: Uart,
    irq_pin: LegacyPin,
}

impl LpcUart {
    pub fn new(sock: Arc<UartSock>, irq_pin: LegacyPin) -> Arc<Self> {
        let mut this = Arc::new(Self {
            state: Mutex::new(UartState { uart: Uart::new(), irq_pin }),
            sock,
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);
        this.sock.on_readable(Arc::downgrade(&this), Self::event_update);
        this
    }
    fn event_update(&self, ctx: &DispCtx) {
        let mut this = self.state.lock().unwrap();
        while let Some(val) = this.uart.data_read() {
            self.sock.write(val, ctx);
        }
        while this.uart.is_writable() {
            if let Some(v) = self.sock.read(ctx) {
                this.uart.data_write(v);
            } else {
                break;
            }
        }
        if this.uart.intr_state() {
            this.irq_pin.assert()
        } else {
            this.irq_pin.deassert()
        }
    }
}
impl SelfArc for LpcUart {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}

impl PioDev for LpcUart {
    fn pio_rw(&self, _port: u16, _ident: usize, rwo: &mut RWOp, ctx: &DispCtx) {
        assert!(rwo.offset() < REGISTER_LEN);
        assert!(rwo.len() != 0);
        let mut this = self.state.lock().unwrap();
        match rwo {
            RWOp::Read(ro) => {
                ro.buf[0] = this.uart.reg_read(ro.offset as u8);
            }
            RWOp::Write(wo) => {
                this.uart.reg_write(wo.offset as u8, wo.buf[0]);
            }
        }
        drop(this);
        self.event_update(ctx);
    }
}

pub struct UartSock {
    inner: Mutex<SockInner>,
    cv: Condvar,
    sa_cell: SelfArcCell<Self>,
}
struct SockInner {
    server: UnixListener,
    state: SockState,
    client: Option<UnixStream>,
    on_readable: Option<(Weak<LpcUart>, fn(&LpcUart, &DispCtx))>,
}
#[derive(Eq, PartialEq)]
enum SockState {
    Bound,
    WaitClient(SubId),
    WaitRead(SubId),
    Readable,
}

impl SelfArc for UartSock {
    fn self_arc_cell(&self) -> &SelfArcCell<Self> {
        &self.sa_cell
    }
}
impl UartSock {
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
            inner: Mutex::new(SockInner {
                server: sock,
                state: SockState::Bound,
                client: None,
                on_readable: None,
            }),
            cv: Condvar::new(),
            sa_cell: SelfArcCell::new(),
        });
        SelfArc::self_arc_init(&mut this);
        Ok(this)
    }
    pub fn accept_ready(&self, ctx: &DispCtx) {
        let mut this = self.inner.lock().unwrap();
        match this.state {
            SockState::WaitClient(_) => {
                // Already subscribed for a client connect event
                return;
            }
            SockState::WaitRead(rsub) => {
                ctx.event.unsubscribe_fd(rsub);
                this.client = None;
            }
            SockState::Readable => {
                this.client = None;
            }
            _ => {}
        }
        let sub = ctx.event.subscribe_fd(
            this.server.as_raw_fd(),
            POLLIN as i32,
            self.self_weak(),
            0,
        );
        this.state = SockState::WaitClient(sub);
    }
    pub fn write(&self, val: u8, ctx: &DispCtx) {
        let mut state = self.inner.lock().unwrap();
        if let Some(client) = state.client.as_mut() {
            if let Err(e) = client.write_all(&[val]) {
                if e.kind() != ErrorKind::WouldBlock {
                    // drop the connection
                    state.client = None
                }
            }
        }
    }
    pub fn read(&self, ctx: &DispCtx) -> Option<u8> {
        let mut this = self.inner.lock().unwrap();
        if this.client.is_none() {
            None
        } else {
            let marked_readable = this.state == SockState::Readable;
            let csock = this.client.as_mut().unwrap();
            let mut val = [0u8];
            if let Err(e) = csock.read(&mut val) {
                if e.kind() != ErrorKind::WouldBlock {
                    // drop the connection
                    this.client = None;
                } else if marked_readable {
                    let sub = ctx.event.subscribe_fd(
                        csock.as_raw_fd(),
                        POLLIN as i32,
                        self.self_weak(),
                        0,
                    );
                    this.state = SockState::WaitRead(sub);
                }
                None
            } else {
                Some(val[0])
            }
        }
    }
    pub fn wait_for_connect(&self) {
        let guard = self
            .cv
            .wait_while(self.inner.lock().unwrap(), |s| s.client.is_none())
            .unwrap();
        // inner mutex uneeded afterwards
        drop(guard);
    }
    fn on_readable(&self, outer: Weak<LpcUart>, cb: fn(&LpcUart, &DispCtx)) {
        let mut this = self.inner.lock().unwrap();
        this.on_readable = Some((outer, cb));
    }
}
impl EventTargetFd for UartSock {
    fn event_handle_fd(
        &self,
        fd: RawFd,
        events: i32,
        _data: usize,
        ctx: &DispCtx,
    ) {
        let mut this = self.inner.lock().unwrap();
        match this.state {
            SockState::WaitClient(_) => {
                assert!(fd == this.server.as_raw_fd());
                let (sock, _addr) = this.server.accept().unwrap();
                // XXX: handle error?
                sock.set_nonblocking(true).unwrap();
                let client_fd = sock.as_raw_fd();
                this.client = Some(sock);
                let sub = ctx.event.subscribe_fd(
                    client_fd,
                    POLLIN as i32,
                    self.self_weak(),
                    0,
                );
                this.state = SockState::WaitRead(sub);
                self.cv.notify_all();
            }
            SockState::WaitRead(_) => {
                assert!(fd == this.client.as_ref().unwrap().as_raw_fd());
                if events & POLLIN as i32 == 0 {
                    // HUP or error
                    this.client = None;
                    this.state = SockState::Bound;
                    self.accept_ready(ctx);
                } else {
                    // actually readable
                    this.state = SockState::Readable;
                    // Call into the outer, if requested
                    if let Some((weak, cbr)) = this.on_readable.as_ref() {
                        let outer = Weak::upgrade(weak).unwrap();
                        let cb = *cbr;
                        // must be done without the lock held
                        drop(this);
                        cb(&outer, ctx);
                    }
                }
            }
            _ => {
                panic!();
            }
        }
    }
}
