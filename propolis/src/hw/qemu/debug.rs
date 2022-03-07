use std::sync::Arc;

use crate::chardev::{BlockingSource, BlockingSourceConsumer, ConsumerCell};
use crate::common::*;
use crate::dispatch::DispCtx;
use crate::migrate::Migrate;
use crate::pio::{PioBus, PioFn};

use erased_serde::Serialize;

const QEMU_DEBUG_IOPORT: u16 = 0x0402;
const QEMU_DEBUG_IDENT: u8 = 0xe9;

pub struct QemuDebugPort {
    consumer: ConsumerCell,
}
impl QemuDebugPort {
    pub fn create(pio: &PioBus) -> Arc<Self> {
        let this = Arc::new(Self { consumer: ConsumerCell::new() });

        let piodev = this.clone();
        let piofn = Arc::new(move |_port: u16, rwo: RWOp, ctx: &DispCtx| {
            piodev.pio_rw(rwo, ctx)
        }) as Arc<PioFn>;
        pio.register(QEMU_DEBUG_IOPORT, 1, piofn).unwrap();
        this
    }

    fn pio_rw(&self, rwo: RWOp, ctx: &DispCtx) {
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

impl Entity for QemuDebugPort {
    fn type_name(&self) -> &'static str {
        "qemu-lpc-debug"
    }

    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}

impl Migrate for QemuDebugPort {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::QemuDebugPortV1 {})
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct QemuDebugPortV1 {}
}
