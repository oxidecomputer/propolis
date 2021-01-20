use std::collections::HashMap;
use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::event_ports::*;
use crate::dispatch::DispCtx;

pub use super::event_ports::FdEvents;

const NOTIFY_TOKEN: usize = 0;

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct Token(usize);

#[derive(Debug)]
pub enum Resource {
    Fd(RawFd, FdEvents),
}

#[derive(Debug)]
pub struct Event {
    pub res: Resource,
    pub token: Token,
}

pub struct EventDispatch {
    eport: EventPort,
    notified: AtomicBool,
    tracker: Mutex<Tracker>,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum WatchState {
    Add,
    Update,
    Remove,
    Rearm,
    Current,
}

struct FdWatch {
    fd: RawFd,
    interest: FdEvents,
    handler: Weak<dyn EventTarget>,
    state: WatchState,
}

struct Tracker {
    next_token: usize,
    updates: VecDeque<Token>,
    fds: HashMap<Token, FdWatch>,
}

impl Tracker {
    fn new() -> Self {
        Self {
            next_token: NOTIFY_TOKEN + 1,
            updates: VecDeque::with_capacity(16),
            fds: HashMap::new(),
        }
    }

    fn dispatch_event(
        mut guard: MutexGuard<Self>,
        token: Token,
        ev: &EventObj,
        ctx: &DispCtx,
    ) {
        match ev {
            EventObj::Fd(fd, revents) => {
                if let Some(mut ent) = guard.fds.get_mut(&token) {
                    let needs_update = match ent.state {
                        WatchState::Remove => {
                            // Registration was removed while we were in the
                            // middle of polling or processing
                            return;
                        }
                        WatchState::Current => {
                            // FD polling is implicitly edge-triggered in event
                            // ports, so force a re-associate to preserve
                            // level-triggered behavior.
                            ent.state = WatchState::Rearm;
                            true
                        }
                        WatchState::Add => {
                            // We should not fire an event on an event which has
                            // not yet been associated with the port
                            panic!("unexpected watch state for event");
                        }
                        _ => false,
                    };
                    if let Some(handler) = Weak::upgrade(&ent.handler) {
                        drop(ent);
                        if needs_update {
                            guard.publish_update(token);
                        }
                        drop(guard);
                        handler.event_process(
                            &Event { res: Resource::Fd(*fd, *revents), token },
                            ctx,
                        );
                    }
                }
            }

            EventObj::User(_, _) => {
                // only expect kick events for now
                assert_eq!(token.0, NOTIFY_TOKEN);
            }
        }
    }

    fn get_next_token(&mut self) -> Token {
        self.next_token += 1;
        Token(self.next_token)
    }

    fn fd_register(
        &mut self,
        fd: RawFd,
        interest: FdEvents,
        handler: Weak<dyn EventTarget>,
    ) -> Token {
        let token = self.get_next_token();

        let res = self.fds.insert(
            token,
            FdWatch { fd, interest, handler, state: WatchState::Add },
        );
        assert!(res.is_none());
        self.publish_update(token);
        token
    }
    fn fd_reregister(&mut self, token: Token, interest: FdEvents) {
        let mut ent = self.fds.get_mut(&token).unwrap();
        ent.interest = interest;
        match ent.state {
            WatchState::Add | WatchState::Update | WatchState::Rearm => {
                // The polling thread has not yet processed a preceeding
                // add/modify action.  It will do so when able.
            }
            WatchState::Current => {
                ent.state = WatchState::Update;
                self.publish_update(token);
            }
            WatchState::Remove => {
                panic!("attempting to alter already-deregistered token");
            }
        }
    }
    fn fd_deregister(&mut self, token: Token) {
        let mut ent = self.fds.get_mut(&token).unwrap();
        match ent.state {
            WatchState::Add => {
                // Addition has not yet been processed, so it can be removed
                // immediately.
                drop(ent);
                let _ent = self.fds.remove(&token).unwrap();
            }
            WatchState::Update => {
                // Update pending, so just change intent to removal.
                ent.state = WatchState::Remove;
            }
            WatchState::Rearm => {
                // Awaiting rearm, so removal from the event port was already
                // (implicitly) done.
                drop(ent);
                let _ent = self.fds.remove(&token).unwrap();
            }
            WatchState::Current => {
                ent.state = WatchState::Remove;
                self.publish_update(token);
            }
            WatchState::Remove => {
                panic!("multiple deregistration calls to token");
            }
        }
    }

    fn publish_update(&mut self, token: Token) {
        self.updates.push_back(token);
    }
    fn apply_updates(&mut self, eport: &EventPort) {
        for token in self.updates.drain(..) {
            if let Some(ent) = self.fds.get_mut(&token) {
                match ent.state {
                    WatchState::Add
                    | WatchState::Update
                    | WatchState::Rearm => {
                        eport
                            .associate(
                                EventObj::Fd(ent.fd, ent.interest),
                                token.0,
                            )
                            .unwrap();
                        ent.state = WatchState::Current;
                    }
                    WatchState::Remove => {
                        eport
                            .dissociate(EventObj::Fd(ent.fd, ent.interest))
                            .unwrap();
                        drop(ent);
                        self.fds.remove(&token).unwrap();
                    }
                    WatchState::Current => {
                        panic!("update requested for unchanged watch");
                    }
                }
            } else {
                // Requests to add and then remove a watch could be issued
                // before the polling thread is able to wake and process them.
                // In such a case, the entry would have been cleaned up already.
            }
        }
    }
}

impl EventDispatch {
    pub fn new() -> Self {
        Self {
            eport: EventPort::new().unwrap(),
            notified: AtomicBool::new(false),
            tracker: Mutex::new(Tracker::new()),
        }
    }

    fn fd_register(
        &self,
        fd: RawFd,
        interest: FdEvents,
        handler: Weak<dyn EventTarget>,
    ) -> Token {
        let mut state = self.tracker.lock().unwrap();
        let token = state.fd_register(fd, interest, handler);
        drop(state);
        self.notify();
        token
    }
    fn fd_reregister(&self, token: Token, interest: FdEvents) {
        let mut state = self.tracker.lock().unwrap();
        state.fd_reregister(token, interest);
        drop(state);
        self.notify();
    }
    fn fd_deregister(&self, token: Token) {
        let mut state = self.tracker.lock().unwrap();
        state.fd_deregister(token);
        drop(state);
        self.notify();
    }

    fn notify(&self) {
        if !self.notified.fetch_or(true, Ordering::SeqCst) {
            self.eport.send(1, NOTIFY_TOKEN).unwrap();
        }
    }

    pub fn process_events(&self, ctx: &DispCtx) {
        self.notified.store(false, Ordering::SeqCst);

        let mut state = self.tracker.lock().unwrap();
        state.apply_updates(&self.eport);
        drop(state);

        let res = self.eport.get();
        if let Err(_e) = res {
            return;
        }
        let event = res.unwrap();

        let state = self.tracker.lock().unwrap();
        Tracker::dispatch_event(state, Token(event.data), &event.obj, ctx);
    }
}

pub trait EventTarget: Send + Sync {
    fn event_process(&self, event: &Event, ctx: &DispCtx);
}

pub struct EventCtx {
    inner: Arc<EventDispatch>,
}

impl EventCtx {
    pub fn new(disp: Arc<EventDispatch>) -> Self {
        Self { inner: disp }
    }
    pub fn fd_register(
        &self,
        fd: RawFd,
        interest: FdEvents,
        handler: Weak<dyn EventTarget>,
    ) -> Token {
        self.inner.fd_register(fd, interest, handler)
    }
    pub fn fd_reregister(&self, token: Token, interest: FdEvents) {
        self.inner.fd_reregister(token, interest)
    }
    pub fn fd_deregister(&self, token: Token) {
        self.inner.fd_deregister(token)
    }
}

pub fn event_loop(edisp: Arc<EventDispatch>, dctx: DispCtx) {
    loop {
        edisp.process_events(&dctx)
    }
}
