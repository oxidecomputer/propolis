use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrate;

use erased_serde::Serialize;

pub struct BhyveHpet {}
impl BhyveHpet {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }
}

impl Entity for BhyveHpet {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-hpet"
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for BhyveHpet {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::BhyveHpet {})
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct BhyveHpet {}
}
