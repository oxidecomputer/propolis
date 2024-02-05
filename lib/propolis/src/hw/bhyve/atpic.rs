// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::common::Lifecycle;
use crate::migrate::*;
use crate::vmm::VmmHdl;

/// Bhyve VMM-emulated PIC (Intel 8259A)
pub struct BhyveAtPic {
    hdl: Arc<VmmHdl>,
}
impl BhyveAtPic {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl Lifecycle for BhyveAtPic {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-atpic"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for BhyveAtPic {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(migrate::AtPicV1::read(&self.hdl)?.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        offer.take::<migrate::AtPicV1>()?.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
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
    impl From<bhyve_api::vdi_atpic_v1> for AtPicV1 {
        fn from(value: bhyve_api::vdi_atpic_v1) -> Self {
            Self { chips: [value.va_chip[0].into(), value.va_chip[1].into()] }
        }
    }
    impl From<AtPicV1> for bhyve_api::vdi_atpic_v1 {
        fn from(value: AtPicV1) -> Self {
            Self { va_chip: [value.chips[0].into(), value.chips[1].into()] }
        }
    }

    impl From<bhyve_api::vdi_atpic_chip_v1> for AtPicChipV1 {
        fn from(value: bhyve_api::vdi_atpic_chip_v1) -> Self {
            Self {
                icw_state: value.vac_icw_state,
                status: value.vac_status,
                reg_irr: value.vac_reg_irr,
                reg_isr: value.vac_reg_isr,
                reg_imr: value.vac_reg_imr,
                irq_base: value.vac_irq_base,
                lowprio: value.vac_lowprio,
                elc: value.vac_elc,
                level: value.vac_level,
            }
        }
    }
    impl From<AtPicChipV1> for bhyve_api::vdi_atpic_chip_v1 {
        fn from(value: AtPicChipV1) -> Self {
            Self {
                vac_icw_state: value.icw_state,
                vac_status: value.status,
                vac_reg_irr: value.reg_irr,
                vac_reg_isr: value.reg_isr,
                vac_reg_imr: value.reg_imr,
                vac_irq_base: value.irq_base,
                vac_lowprio: value.lowprio,
                vac_elc: value.elc,
                vac_level: value.level,
            }
        }
    }

    impl AtPicV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi = hdl
                .data_op(bhyve_api::VDC_ATPIC, 1)
                .read::<bhyve_api::vdi_atpic_v1>()?;

            Ok(vdi.into())
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            hdl.data_op(bhyve_api::VDC_ATPIC, 1)
                .write::<bhyve_api::vdi_atpic_v1>(&self.into())?;

            Ok(())
        }
    }
    impl Schema<'_> for AtPicV1 {
        fn id() -> SchemaId {
            ("bhyve-atpic", 1)
        }
    }
}
