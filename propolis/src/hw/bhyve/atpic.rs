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
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        Box::new(migrate::BhyveAtPicV1::read(hdl))
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::Serialize;

    #[derive(Copy, Clone, Default, Serialize)]
    pub struct BhyveAtPicV1 {
        /// XXX: do not expose vdi struct
        pub data: bhyve_api::vdi_atpic_v1,
    }

    impl BhyveAtPicV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            Self {
                data: vmm::data::read(hdl, -1, bhyve_api::VDC_ATPIC, 1)
                    .unwrap(),
            }
        }
    }
}
