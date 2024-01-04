// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{
    atomic::{AtomicUsize, Ordering::Relaxed},
    Arc,
};

use crate::common::*;
use crate::pio::{PioBus, PioFn};

#[derive(Clone)]
pub struct QemuPioPvpanic(Arc<CountsInner>);

pub struct PvpanicCounts(Arc<CountsInner>);

struct CountsInner {
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

impl PvpanicCounts {
    pub fn new() -> Self {
        Self(Arc::new(CountsInner {
            host_handled: AtomicUsize::new(0),
            guest_handled: AtomicUsize::new(0),
        }))
    }
}

impl QemuPioPvpanic {
    const IOPORT: u16 = 0x505;

    pub fn create(
        &PvpanicCounts(ref counts): &PvpanicCounts,
        pio: &PioBus,
    ) -> Self {
        let this = Self(counts.clone());

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
                let c = wo.read_u8();
                if c & HOST_HANDLED != 0 {
                    self.0.host_handled.fetch_add(1, Relaxed);
                }

                if c & GUEST_HANDLED != 0 {
                    self.0.guest_handled.fetch_add(1, Relaxed);
                }
            }
        }
    }
}

impl Entity for QemuPioPvpanic {
    fn type_name(&self) -> &'static str {
        "qemu-pvpanic-pio"
    }
}
