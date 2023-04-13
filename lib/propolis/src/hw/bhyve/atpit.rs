use std::sync::Arc;

use crate::inventory::Entity;
use crate::migrate::*;
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

pub struct BhyveAtPit {
    hdl: Arc<VmmHdl>,
}
impl BhyveAtPit {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
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
    fn export(&self, _ctx: &MigrateCtx) -> Box<dyn Serialize> {
        Box::new(migrate::AtPitV1::read(&self.hdl).unwrap())
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let deserialized: migrate::AtPitV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct AtPitV1 {
        pub channel: [AtPitChannelV1; 3],
    }

    #[derive(Copy, Clone, Default, Serialize, Deserialize)]
    pub struct AtPitChannelV1 {
        pub initial: u16,
        pub reg_cr: u16,
        pub reg_ol: u16,
        pub reg_status: u8,
        pub mode: u8,
        pub status: u8,
        pub time_target: i64,
    }
    impl AtPitV1 {
        fn from_raw(inp: &bhyve_api::vdi_atpit_v1) -> Self {
            Self {
                channel: [
                    AtPitChannelV1::from_raw(&inp.va_channel[0]),
                    AtPitChannelV1::from_raw(&inp.va_channel[1]),
                    AtPitChannelV1::from_raw(&inp.va_channel[2]),
                ],
            }
        }
        fn to_raw(&self) -> bhyve_api::vdi_atpit_v1 {
            bhyve_api::vdi_atpit_v1 {
                va_channel: [
                    self.channel[0].to_raw(),
                    self.channel[1].to_raw(),
                    self.channel[2].to_raw(),
                ],
            }
        }
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi: bhyve_api::vdi_atpit_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_ATPIT, 1)?;

            Ok(Self::from_raw(&vdi))
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            vmm::data::write(hdl, -1, bhyve_api::VDC_ATPIT, 1, self.to_raw())?;
            Ok(())
        }
    }
    impl AtPitChannelV1 {
        fn from_raw(inp: &bhyve_api::vdi_atpit_channel_v1) -> Self {
            Self {
                initial: inp.vac_initial,
                reg_cr: inp.vac_reg_cr,
                reg_ol: inp.vac_reg_ol,
                reg_status: inp.vac_reg_status,
                mode: inp.vac_mode,
                status: inp.vac_status,
                time_target: inp.vac_time_target,
            }
        }
        fn to_raw(&self) -> bhyve_api::vdi_atpit_channel_v1 {
            bhyve_api::vdi_atpit_channel_v1 {
                vac_initial: self.initial,
                vac_reg_cr: self.reg_cr,
                vac_reg_ol: self.reg_ol,
                vac_reg_status: self.reg_status,
                vac_mode: self.mode,
                vac_status: self.status,
                vac_time_target: self.time_target,
            }
        }
    }
}
