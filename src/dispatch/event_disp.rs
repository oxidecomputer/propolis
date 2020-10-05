use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex, Weak};

use super::event_ports::*;
use crate::dispatch::DispCtx;

const EPORT_NOTIFY_USER: usize = 0;

type SubId = usize;

pub struct EventDispatch {
    eport: EventPort,
    inner: Mutex<Inner>,
}

struct Inner {
    next_sub_id: usize,
    fd_subs: HashMap<SubId, FdSub>,
    fd_add: HashMap<SubId, FdSub>,
    fd_del: HashMap<SubId, FdSub>,
}

impl Inner {
    fn new(first_id: SubId) -> Self {
        Self {
            next_sub_id: first_id,
            fd_subs: HashMap::new(),
            fd_add: HashMap::new(),
            fd_del: HashMap::new(),
        }
    }
    fn process_changes(&mut self, eport: &EventPort) {
        for (id, sub) in self.fd_add.drain() {
            let events = FdEvents::from_bits_truncate(sub.events);
            eport.associate(EventObj::Fd(sub.fd, events), id).unwrap();
        }
        for (_id, sub) in self.fd_del.drain() {
            eport.dissociate(EventObj::Fd(sub.fd, FdEvents::empty())).unwrap();
        }
    }
}

impl EventDispatch {
    pub fn new() -> Self {
        let eport = EventPort::new().unwrap();
        Self { inner: Mutex::new(Inner::new(EPORT_NOTIFY_USER + 1)), eport }
    }

    pub fn subscribe_fd(
        &self,
        fd: RawFd,
        events: i32,
        handler: Weak<dyn EventTargetFd>,
        data: usize,
    ) -> SubId {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_sub_id;
        inner.next_sub_id += 1;
        inner.fd_add.insert(id, FdSub { fd, events, handler, data });
        self.kick();
        drop(inner);
        id
    }
    pub fn unsubscribe_fd(&self, id: SubId) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(sub) = inner.fd_subs.remove(&id) {
            inner.fd_del.insert(id, sub);
            self.kick();
        }
    }
    pub fn event_loop(&self, ctx: &DispCtx) {
        let mut inner = self.inner.lock().unwrap();
        inner.process_changes(&self.eport);
        drop(inner);
        let res = self.eport.get();
        if let Err(_e) = res {
            return;
        }
        let event = res.unwrap();
        let mut inner = self.inner.lock().unwrap();
        match event.obj {
            EventObj::Fd(fd, revent) => {
                if let Some(sub) = inner.fd_subs.remove(&event.data) {
                    assert_eq!(fd, sub.fd);
                    if let Some(handler) = Weak::upgrade(&sub.handler) {
                        drop(inner);
                        // inner lock must not be held across handler call
                        handler.event_handle_fd(
                            sub.fd,
                            revent.bits(),
                            sub.data,
                            ctx,
                        );
                        return;
                    }
                }
            }
            EventObj::User(_arb, _revent) => {
                // only expect kick events for now
                assert_eq!(event.data, EPORT_NOTIFY_USER);
                return;
            }
        }
    }

    fn kick(&self) {
        self.eport.send(1, EPORT_NOTIFY_USER).unwrap();
    }
}

struct FdSub {
    fd: RawFd,
    events: i32,
    handler: Weak<dyn EventTargetFd>,
    data: usize,
}

pub trait EventTargetFd: Send + Sync {
    fn event_handle_fd(
        &self,
        fd: RawFd,
        events: i32,
        data: usize,
        ctx: &DispCtx,
    );
}

pub struct EventCtx {
    inner: Arc<EventDispatch>,
}

impl EventCtx {
    pub fn new(disp: Arc<EventDispatch>) -> Self {
        Self { inner: disp }
    }
    pub fn subscribe_fd(
        &self,
        fd: RawFd,
        events: i32,
        handler: Weak<dyn EventTargetFd>,
        data: usize,
    ) -> SubId {
        self.inner.subscribe_fd(fd, events, handler, data)
    }
    pub fn unsubscribe_fd(&self, id: SubId) {
        self.inner.unsubscribe_fd(id)
    }
}

pub fn event_loop(dctx: DispCtx, edisp: Arc<EventDispatch>) {
    loop {
        edisp.event_loop(&dctx)
    }
}
