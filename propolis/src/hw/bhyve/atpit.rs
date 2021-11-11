use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrate;

use erased_serde::Serialize;

pub struct BhyveAtPit {}
impl BhyveAtPit {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }
}

impl Entity for BhyveAtPit {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-atpit"
    }
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for BhyveAtPit {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::BhyveAtPitV1 {})
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct BhyveAtPitV1 {}
}
