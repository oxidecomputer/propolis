// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::chardev::{BlockingSource, BlockingSourceConsumer, ConsumerCell};
use crate::common::*;
use crate::pio::{PioBus, PioFn};

const QEMU_PVPANIC_IOPORT: u16 = 0x505;

/// Indicates that a guest panic has happened and should be processed by the
/// host
const HOST_HANDLED: u8 = 0b01;
/// Indicates a guest panic has happened and will be handled by the guest; the
/// host should record it or report it, but should not affect the execution of
/// the guest.
const GUEST_HANDLED: u8 = 0b10;

pub struct QemuPioPvpanic {
    consumer: ConsumerCell,
}

impl QemuPioPvpanic {
    pub fn create(pio: &PioBus) -> Arc<Self> {
        let this = Arc::new(Self { consumer: ConsumerCell::new() });

        let piodev = this.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp| piodev.pio_rw(rwo))
            as Arc<PioFn>;
        pio.register(QEMU_PVPANIC_IOPORT, 1, piofn).unwrap();
        this
    }

    fn pio_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(HOST_HANDLED | GUEST_HANDLED);
            }
            RWOp::Write(wo) => {
                let c = wo.read_u8();
                self.consumer.consume(&[c]);
            }
        }
    }
}

impl BlockingSource for QemuDebugPort {
    fn set_consumer(&self, f: Option<BlockingSourceConsumer>) {
        self.consumer.set(f);
    }
}

impl Entity for QemuDebugPort {
    fn type_name(&self) -> &'static str {
        "qemu-lpc-debug"
    }
}
