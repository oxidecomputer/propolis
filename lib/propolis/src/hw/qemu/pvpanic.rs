// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{
    atomic::{AtomicUsize, Ordering::Relaxed},
    Arc,
};

use crate::common::*;
use crate::pio::{PioBus, PioFn};

pub struct QemuPioPvpanic {
    counts: Arc<PanicCounts>,
}

#[derive(Debug, Default)]
pub struct PanicCounts {
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

impl QemuPioPvpanic {
    const IOPORT: u16 = 0x505;

    #[must_use]
    pub fn create(pio: &PioBus, counts: Arc<PanicCounts>) -> Arc<Self> {
        let this = Arc::new(Self { counts });

        let piodev = this.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp| piodev.pio_rw(rwo))
            as Arc<PioFn>;
        pio.register(Self::IOPORT, 1, piofn).unwrap();
        this
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
                    self.counts.host_handled.fetch_add(1, Relaxed);
                }

                if value & GUEST_HANDLED != 0 {
                    self.counts.guest_handled.fetch_add(1, Relaxed);
                }
            }
        }
    }
}

impl PanicCounts {
    pub fn host_handled_count(&self) -> usize {
        self.host_handled.load(Relaxed)
    }

    pub fn guest_handled_count(&self) -> usize {
        self.guest_handled.load(Relaxed)
    }
}

impl Entity for QemuPioPvpanic {
    fn type_name(&self) -> &'static str {
        "qemu-pvpanic-pio"
    }
}
