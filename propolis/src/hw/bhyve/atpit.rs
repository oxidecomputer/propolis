use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, Migrator};

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
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for BhyveAtPit {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        Box::new(migrate::BhyveAtPitV1::read(hdl))
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::Serialize;

    #[derive(Copy, Clone, Default, Serialize)]
    pub struct BhyveAtPitV1 {
        /// XXX: do not expose vdi struct
        pub data: bhyve_api::vdi_atpit_v1,
    }

    impl BhyveAtPitV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            Self {
                data: vmm::data::read(hdl, -1, bhyve_api::VDC_ATPIT, 1)
                    .unwrap(),
            }
        }
    }
}
