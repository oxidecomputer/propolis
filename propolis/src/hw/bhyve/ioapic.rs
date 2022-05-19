use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, Migrator};

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
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for BhyveIoApic {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        Box::new(migrate::BhyveIoApicV1::read(hdl))
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::Serialize;

    #[derive(Copy, Clone, Default, Serialize)]
    pub struct BhyveIoApicV1 {
        pub id: u32,
        pub reg_sel: u32,
        pub registers: [u64; 32],
        pub levels: [u32; 32],
    }

    impl BhyveIoApicV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            let vdi: bhyve_api::vdi_ioapic_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_IOAPIC, 1).unwrap();

            Self {
                id: vdi.vi_id,
                reg_sel: vdi.vi_reg_sel,
                registers: vdi.vi_pin_reg,
                levels: vdi.vi_pin_level,
            }
        }
    }
}
