use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrate;

use erased_serde::Serialize;

pub struct BhyveIoApic {}
impl BhyveIoApic {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }
}

impl Entity for BhyveIoApic {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-ioapic"
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for BhyveIoApic {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        // TODO: impl export
        Box::new(migrate::BhyveIoApicV1::default())
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize, Default)]
    pub struct BhyveIoApicV1 {
        pub reg_sel: u32,
        pub registers: [u64; 32],
        pub levels: [u32; 32],
    }
}
