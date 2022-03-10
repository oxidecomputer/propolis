use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, Migrator};

use erased_serde::Serialize;

pub struct BhyveAtPic {}
impl BhyveAtPic {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }
}

impl Entity for BhyveAtPic {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-atpic"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for BhyveAtPic {
    fn export(&self, _ctx: &DispCtx) -> Box<dyn Serialize> {
        Box::new(migrate::BhyveAtPicV1 {})
    }
}

pub mod migrate {
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct BhyveAtPicV1 {}
}
