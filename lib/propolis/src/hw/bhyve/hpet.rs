use std::sync::Arc;

use crate::inventory::Entity;
use crate::migrate::*;
use crate::vmm::VmmHdl;

pub struct BhyveHpet {
    hdl: Arc<VmmHdl>,
}
impl BhyveHpet {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl Entity for BhyveHpet {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-hpet"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Single(self)
    }
}
impl MigrateSingle for BhyveHpet {
    fn export(
        &self,
        _ctx: &MigrateCtx,
    ) -> Result<PayloadOutput, MigrateStateError> {
        Ok(migrate::HpetV1::read(&self.hdl)?.emit())
    }

    fn import(
        &self,
        mut offer: PayloadOffer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        offer.parse::<migrate::HpetV1>()?.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Serialize, Deserialize)]
    pub struct HpetV1 {
        pub config: u64,
        pub isr: u64,
        pub count_base: u32,
        pub time_base: i64,

        pub timers: [HpetTimerV1; 8],
    }

    #[derive(Copy, Clone, Default, Serialize, Deserialize)]
    pub struct HpetTimerV1 {
        pub config: u64,
        pub msi: u64,
        pub comp_val: u32,
        pub comp_rate: u32,
        pub time_target: i64,
    }

    impl HpetV1 {
        fn from_raw(inp: &bhyve_api::vdi_hpet_v1) -> Self {
            let mut res = Self {
                config: inp.vh_config,
                isr: inp.vh_isr,
                count_base: inp.vh_count_base,
                time_base: inp.vh_time_base,
                ..Default::default()
            };
            for (n, timer) in res.timers.iter_mut().enumerate() {
                *timer = HpetTimerV1::from_raw(&inp.vh_timers[n]);
            }
            res
        }
        fn to_raw(&self) -> bhyve_api::vdi_hpet_v1 {
            let mut res = bhyve_api::vdi_hpet_v1 {
                vh_config: self.config,
                vh_isr: self.isr,
                vh_count_base: self.count_base,
                vh_time_base: self.time_base,
                ..Default::default()
            };
            for (n, timer) in res.vh_timers.iter_mut().enumerate() {
                *timer = HpetTimerV1::to_raw(&self.timers[n]);
            }
            res
        }
    }
    impl HpetTimerV1 {
        fn from_raw(inp: &bhyve_api::vdi_hpet_timer_v1) -> Self {
            Self {
                config: inp.vht_config,
                msi: inp.vht_msi,
                comp_val: inp.vht_comp_val,
                comp_rate: inp.vht_comp_rate,
                time_target: inp.vht_time_target,
            }
        }
        fn to_raw(&self) -> bhyve_api::vdi_hpet_timer_v1 {
            bhyve_api::vdi_hpet_timer_v1 {
                vht_config: self.config,
                vht_msi: self.msi,
                vht_comp_val: self.comp_val,
                vht_comp_rate: self.comp_rate,
                vht_time_target: self.time_target,
            }
        }
    }

    impl HpetV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi: bhyve_api::vdi_hpet_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_HPET, 1)?;

            Ok(Self::from_raw(&vdi))
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            vmm::data::write(hdl, -1, bhyve_api::VDC_HPET, 1, self.to_raw())?;
            Ok(())
        }
    }
    impl Schema<'_> for HpetV1 {
        fn id() -> SchemaId {
            ("bhyve-hpet", 1)
        }
    }
}
