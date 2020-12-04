use std::io::Write;
use std::sync::{Arc, Mutex, Weak};

use crate::common::*;
use crate::dispatch::DispCtx;
use crate::pio::{PioBus, PioDev};

const QEMU_DEBUG_IOPORT: u16 = 0x0402;
const QEMU_DEBUG_IDENT: u8 = 0xe9;

pub struct QemuDebugPort {
    out: Option<Mutex<Box<dyn Write + Send>>>,
}
impl QemuDebugPort {
    pub fn create(
        outf: Option<Box<dyn Write + Send>>,
        pio: &PioBus,
    ) -> Arc<Self> {
        let this = Arc::new(Self { out: outf.map(Mutex::new) });
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
    fn pio_rw(
        &self,
        _port: u16,
        _ident: usize,
        rwo: &mut RWOp,
        _ctx: &DispCtx,
    ) {
        match rwo {
            RWOp::Read(ro) => {
                assert!(!ro.buf.is_empty());
                ro.buf[0] = QEMU_DEBUG_IDENT;
            }
            RWOp::Write(wo) => {
                if let Some(out) = self.out.as_ref() {
                    let mut locked = out.lock().unwrap();
                    let _ = locked.write_all(&wo.buf);
                    if wo.buf[0] == b'\n' {
                        let _ = locked.flush();
                    }
                }
            }
        }
    }
}
