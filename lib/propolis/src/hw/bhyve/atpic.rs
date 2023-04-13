use std::sync::Arc;

use crate::inventory::Entity;
use crate::migrate::*;
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

pub struct BhyveAtPic {
    hdl: Arc<VmmHdl>,
}
impl BhyveAtPic {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
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
    fn export(&self, _ctx: &MigrateCtx) -> Box<dyn Serialize> {
        Box::new(migrate::AtPicV1::read(&self.hdl).unwrap())
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let deserialized: migrate::AtPicV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct AtPicV1 {
        pub chips: [AtPicChipV1; 2],
    }
    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct AtPicChipV1 {
        pub icw_state: u8,
        pub status: u8,
        pub reg_irr: u8,
        pub reg_isr: u8,
        pub reg_imr: u8,
        pub irq_base: u8,
        pub lowprio: u8,
        pub elc: u8,
        pub level: [u32; 8],
    }
    impl AtPicV1 {
        fn from_raw(inp: &bhyve_api::vdi_atpic_v1) -> Self {
            Self {
                chips: [
                    AtPicChipV1::from_raw(&inp.va_chip[0]),
                    AtPicChipV1::from_raw(&inp.va_chip[1]),
                ],
            }
        }
        fn to_raw(&self) -> bhyve_api::vdi_atpic_v1 {
            bhyve_api::vdi_atpic_v1 {
                va_chip: [self.chips[0].to_raw(), self.chips[1].to_raw()],
            }
        }
    }
    impl AtPicChipV1 {
        fn from_raw(inp: &bhyve_api::vdi_atpic_chip_v1) -> Self {
            Self {
                icw_state: inp.vac_icw_state,
                status: inp.vac_status,
                reg_irr: inp.vac_reg_irr,
                reg_isr: inp.vac_reg_isr,
                reg_imr: inp.vac_reg_imr,
                irq_base: inp.vac_irq_base,
                lowprio: inp.vac_lowprio,
                elc: inp.vac_elc,
                level: inp.vac_level,
            }
        }
        fn to_raw(&self) -> bhyve_api::vdi_atpic_chip_v1 {
            bhyve_api::vdi_atpic_chip_v1 {
                vac_icw_state: self.icw_state,
                vac_status: self.status,
                vac_reg_irr: self.reg_irr,
                vac_reg_isr: self.reg_isr,
                vac_reg_imr: self.reg_imr,
                vac_irq_base: self.irq_base,
                vac_lowprio: self.lowprio,
                vac_elc: self.elc,
                vac_level: self.level,
            }
        }
    }

    impl AtPicV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi: bhyve_api::vdi_atpic_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_ATPIC, 1)?;

            Ok(Self::from_raw(&vdi))
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            vmm::data::write(hdl, -1, bhyve_api::VDC_ATPIC, 1, self.to_raw())?;
            Ok(())
        }
    }
}
