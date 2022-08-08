use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, MigrateStateError, Migrator};

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

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        ctx: &DispCtx,
    ) -> Result<(), MigrateStateError> {
        let deserialized: migrate::BhyvePmTimerV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(ctx.mctx.hdl())?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Default, Deserialize, Serialize)]
    pub struct BhyvePmTimerV1 {
        pub start_time: i64,
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

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            let vdi = bhyve_api::vdi_pm_timer_v1 {
                vpt_time_base: self.start_time,
                vpt_ioport: 0, // TODO: is this right?
            };
            vmm::data::write(hdl, -1, bhyve_api::VDC_PM_TIMER, 1, vdi)?;
            Ok(())
        }
    }
}
