use std::sync::Arc;

use crate::inventory::Entity;
use crate::migrate::*;
use crate::vmm::VmmHdl;

pub struct BhyvePmTimer {
    hdl: Arc<VmmHdl>,
}
impl BhyvePmTimer {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl Entity for BhyvePmTimer {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-pmtimer"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Single(self)
    }
}
impl MigrateSingle for BhyvePmTimer {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        Ok(migrate::BhyvePmTimerV1::read(&self.hdl)?.emit())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        offer.parse::<migrate::BhyvePmTimerV1>()?.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Default, Deserialize, Serialize)]
    pub struct BhyvePmTimerV1 {
        pub start_time: i64,
    }
    impl BhyvePmTimerV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi: bhyve_api::vdi_pm_timer_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_PM_TIMER, 1)?;

            Ok(Self {
                // vdi_pm_timer_v1 also carries the ioport to which the pmtimer
                // is attached, but migration of that state is handled by the
                // chipset PM device.
                start_time: vdi.vpt_time_base,
            })
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
    impl Schema<'_> for BhyvePmTimerV1 {
        fn id() -> SchemaId {
            ("bhyve-atpic", 1)
        }
    }
}
