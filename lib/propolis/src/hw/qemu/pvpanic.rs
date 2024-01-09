// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{
    atomic::{AtomicUsize, Ordering::Relaxed},
    Arc,
};

use crate::common::*;
use crate::pio::{PioBus, PioFn};

#[derive(Debug)]
pub struct QemuPvpanic {
    host_handled: AtomicUsize,
    guest_handled: AtomicUsize,
}

/// Indicates that a guest panic has happened and should be processed by the
/// host
const HOST_HANDLED: u8 = 0b01;
/// Indicates a guest panic has happened and will be handled by the guest; the
/// host should record it or report it, but should not affect the execution of
/// the guest.
const GUEST_HANDLED: u8 = 0b10;

#[usdt::provider(provider = "propolis")]
mod probes {
    fn pvpanic_pio_write(value: u8) {}
}

impl QemuPvpanic {
    const IOPORT: u16 = 0x505;

    pub fn create() -> Arc<Self> {
        Arc::new(Self {
            host_handled: AtomicUsize::new(0),
            guest_handled: AtomicUsize::new(0),
        })
    }

    pub fn attach_pio(self: &Arc<Self>, pio: &PioBus) {
        let piodev = self.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp| piodev.pio_rw(rwo))
            as Arc<PioFn>;
        pio.register(Self::IOPORT, 1, piofn).unwrap();
    }

    pub fn host_handled_count(&self) -> usize {
        self.host_handled.load(Relaxed)
    }

    pub fn guest_handled_count(&self) -> usize {
        self.guest_handled.load(Relaxed)
    }

    fn pio_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(HOST_HANDLED | GUEST_HANDLED);
            }
            RWOp::Write(wo) => {
                let value = wo.read_u8();
                probes::pvpanic_pio_write!(|| value);
                if value & HOST_HANDLED != 0 {
                    self.host_handled.fetch_add(1, Relaxed);
                }

                if value & GUEST_HANDLED != 0 {
                    self.guest_handled.fetch_add(1, Relaxed);
                }
            }
        }
    }
}

impl Entity for QemuPvpanic {
    fn type_name(&self) -> &'static str {
        "qemu-pvpanic"
    }
}
