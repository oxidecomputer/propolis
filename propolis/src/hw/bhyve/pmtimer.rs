use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrate;

use erased_serde::Serialize;

pub struct BhyvePmTimer {}
impl BhyvePmTimer {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }
}

impl Entity for BhyvePmTimer {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-pmtimer"
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for BhyvePmTimer {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        // TODO: impl export
        Box::new(migrate::BhyvePmTimerV1::default())
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize, Default)]
    pub struct BhyvePmTimerV1 {
        pub start_time: u64,
        pub start_val: u32,
    }
}
