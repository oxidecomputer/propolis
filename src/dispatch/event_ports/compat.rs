#![allow(unused)]
use std::io::{Error, ErrorKind, Result};

use super::*;

pub struct EventPort {}

impl EventPort {
    pub fn new() -> Result<Self> {
        Err(Self::not_impl_err())
    }
    /// Associate an event object with this port.
    pub fn associate(&self, event: EventObj, udata: usize) -> Result<()> {
        Err(Self::not_impl_err())
    }
    /// Dissociate an event object from this port.
    pub fn dissociate(&self, event: EventObj) -> Result<()> {
        Err(Self::not_impl_err())
    }
    /// Block for an event from this port.
    pub fn get(&self) -> Result<EventResult> {
        Err(Self::not_impl_err())
    }
    /// Send an event to a resource associated with the port.
    pub fn send(&self, events: i32, udata: usize) -> Result<()> {
        Err(Self::not_impl_err())
    }

    fn not_impl_err() -> Error {
        Error::new(ErrorKind::Other, "for check purposes only on non-illumos")
    }
}
