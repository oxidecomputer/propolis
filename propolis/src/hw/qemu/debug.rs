use std::sync::{Arc, Weak};

use crate::chardev::{BlockingSource, BlockingSourceConsumer, ConsumerCell};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pio::{PioBus, PioDev};

const QEMU_DEBUG_IOPORT: u16 = 0x0402;
const QEMU_DEBUG_IDENT: u8 = 0xe9;

pub struct QemuDebugPort {
    consumer: ConsumerCell,
}
impl QemuDebugPort {
    pub fn create(pio: &PioBus) -> Arc<Self> {
        let this = Arc::new(Self { consumer: ConsumerCell::new() });

        pio.register(
            QEMU_DEBUG_IOPORT,
            1,
            Arc::downgrade(&this) as Weak<dyn PioDev>,
            0,
        )
        .unwrap();
        this
    }
}

impl PioDev for QemuDebugPort {
    fn pio_rw(&self, _port: u16, _ident: usize, rwo: RWOp, ctx: &DispCtx) {
        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(QEMU_DEBUG_IDENT);
            }
            RWOp::Write(wo) => {
                let c = wo.read_u8();
                self.consumer.consume(&[c], ctx);
            }
        }
    }
}

impl BlockingSource for QemuDebugPort {
    fn set_consumer(&self, f: Option<BlockingSourceConsumer>) {
        self.consumer.set(f);
    }
}

impl Entity for QemuDebugPort {}
