// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::chardev::{BlockingSource, BlockingSourceConsumer, ConsumerCell};
use crate::common::*;
use crate::pio::{PioBus, PioFn};

const QEMU_DEBUG_IOPORT: u16 = 0x0402;
const QEMU_DEBUG_IDENT: u8 = 0xe9;

pub struct QemuDebugPort {
    consumer: ConsumerCell,
}
impl QemuDebugPort {
    pub fn create(pio: &PioBus) -> Arc<Self> {
        let this = Arc::new(Self { consumer: ConsumerCell::new() });

        let piodev = this.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp| piodev.pio_rw(rwo))
            as Arc<PioFn>;
        pio.register(QEMU_DEBUG_IOPORT, 1, piofn).unwrap();
        this
    }

    fn pio_rw(&self, rwo: RWOp) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(QEMU_DEBUG_IDENT);
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

impl Lifecycle for QemuDebugPort {
    fn type_name(&self) -> &'static str {
        "qemu-lpc-debug"
    }
}
