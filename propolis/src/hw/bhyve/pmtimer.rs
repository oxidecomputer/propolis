use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, Migrator};

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
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for BhyvePmTimer {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        Box::new(migrate::BhyvePmTimerV1::read(hdl))
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::Serialize;

    #[derive(Serialize, Default)]
    pub struct BhyvePmTimerV1 {
        pub start_time: u64,
    }
    impl BhyvePmTimerV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            let vdi: bhyve_api::vdi_pm_timer_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_PM_TIMER, 1).unwrap();

            Self {
                // vdi_pm_timer_v1 also carries the ioport to which the pmtimer
                // is attached, but migration of that state is handled by the
                // chipset PM device.
                start_time: vdi.vpt_time_base,
            }
        }
    }
}
