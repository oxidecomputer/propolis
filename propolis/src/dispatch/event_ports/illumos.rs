use std::io::{Error, Result};
use std::mem::transmute;
use std::os::unix::io::RawFd;
use std::ptr;

use libc::close;
use libc::{c_void, port_event};
use libc::{port_associate, port_create, port_dissociate, port_get, port_send};
use libc::{PORT_SOURCE_FD, PORT_SOURCE_USER};

use super::*;

pub struct EventPort {
    fd: RawFd,
}

impl EventPort {
    pub fn new() -> Result<Self> {
        let fd = unsafe { port_create() };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        Ok(Self { fd })
    }
    /// Associate an event object with this port.
    pub fn associate(&self, event: EventObj, udata: usize) -> Result<()> {
        match event {
            EventObj::Fd(fd, ev) => {
                assert!(!ev.contains(
                    FdEvents::POLLERR | FdEvents::POLLHUP | FdEvents::POLLNVAL,
                ));
                self.raw_associate(
                    PORT_SOURCE_FD,
                    fd as usize,
                    ev.bits(),
                    udata,
                )
            }
            EventObj::User(arb, ev) => {
                self.raw_associate(PORT_SOURCE_USER, arb, ev, udata)
            }
        }
    }
    /// Dissociate an event object from this port.
    pub fn dissociate(&self, event: EventObj) -> Result<()> {
        match event {
            EventObj::Fd(fd, _ev) => {
                self.raw_dissociate(PORT_SOURCE_FD, fd as usize)
            }
            EventObj::User(arb, _ev) => {
                self.raw_dissociate(PORT_SOURCE_USER, arb)
            }
        }
    }
    /// Block for an event from this port.
    pub fn get(&self) -> Result<EventResult> {
        let mut raw_event = port_event {
            portev_events: 0,
            portev_source: 0,
            portev_pad: 0,
            portev_object: 0,
            portev_user: ptr::null_mut(),
        };
        let rv = unsafe { port_get(self.fd, &mut raw_event, ptr::null_mut()) };
        if rv < 0 {
            return Err(Error::last_os_error());
        }
        let ev = match raw_event.portev_source as i32 {
            PORT_SOURCE_FD => {
                let fdev =
                    FdEvents::from_bits(raw_event.portev_events).unwrap();
                EventObj::Fd(raw_event.portev_object as RawFd, fdev)
            }
            PORT_SOURCE_USER => {
                EventObj::User(raw_event.portev_object, raw_event.portev_events)
            }
            _ => panic!(),
        };
        Ok(EventResult { obj: ev, data: raw_event.portev_user as usize })
    }
    /// Send an event to a resource associated with the port.
    pub fn send(&self, events: i32, udata: usize) -> Result<()> {
        let rv = unsafe { port_send(self.fd, events, transmute(udata)) };
        if rv < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    fn raw_associate(
        &self,
        source: i32,
        obj: usize,
        events: i32,
        data: usize,
    ) -> Result<()> {
        let rv = unsafe {
            let dataptr: *mut c_void = transmute(data);
            port_associate(self.fd, source, obj, events, dataptr)
        };
        if rv < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn raw_dissociate(&self, source: i32, obj: usize) -> Result<()> {
        let rv = unsafe { port_dissociate(self.fd, source, obj) };
        if rv < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for EventPort {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}
