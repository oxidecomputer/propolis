// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::common::Lifecycle;
use crate::migrate::*;
use crate::vmm::VmmHdl;

/// Bhyve VMM-emulated HPET
pub struct BhyveHpet {
    hdl: Arc<VmmHdl>,
}
impl BhyveHpet {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl Lifecycle for BhyveHpet {
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
        Ok(migrate::HpetV1::read(&self.hdl)?.into())
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
    impl From<bhyve_api::vdi_hpet_v1> for HpetV1 {
        fn from(value: bhyve_api::vdi_hpet_v1) -> Self {
            Self {
                config: value.vh_config,
                isr: value.vh_isr,
                count_base: value.vh_count_base,
                time_base: value.vh_time_base,
                timers: value.vh_timers.map(Into::into),
            }
        }
    }
    impl From<HpetV1> for bhyve_api::vdi_hpet_v1 {
        fn from(value: HpetV1) -> Self {
            Self {
                vh_config: value.config,
                vh_isr: value.isr,
                vh_count_base: value.count_base,
                vh_time_base: value.time_base,
                vh_timers: value.timers.map(Into::into),
            }
        }
    }

    #[derive(Copy, Clone, Default, Serialize, Deserialize)]
    pub struct HpetTimerV1 {
        pub config: u64,
        pub msi: u64,
        pub comp_val: u32,
        pub comp_rate: u32,
        pub time_target: i64,
    }
    impl From<bhyve_api::vdi_hpet_timer_v1> for HpetTimerV1 {
        fn from(value: bhyve_api::vdi_hpet_timer_v1) -> Self {
            Self {
                config: value.vht_config,
                msi: value.vht_msi,
                comp_val: value.vht_comp_val,
                comp_rate: value.vht_comp_rate,
                time_target: value.vht_time_target,
            }
        }
    }
    impl From<HpetTimerV1> for bhyve_api::vdi_hpet_timer_v1 {
        fn from(value: HpetTimerV1) -> Self {
            Self {
                vht_config: value.config,
                vht_msi: value.msi,
                vht_comp_val: value.comp_val,
                vht_comp_rate: value.comp_rate,
                vht_time_target: value.time_target,
            }
        }
    }

    impl HpetV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi = hdl
                .data_op(bhyve_api::VDC_HPET, 1)
                .read::<bhyve_api::vdi_hpet_v1>()?;

            Ok(vdi.into())
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            hdl.data_op(bhyve_api::VDC_HPET, 1)
                .write::<bhyve_api::vdi_hpet_v1>(&self.into())?;

            Ok(())
        }
    }
    impl Schema<'_> for HpetV1 {
        fn id() -> SchemaId {
            ("bhyve-hpet", 1)
        }
    }
}
