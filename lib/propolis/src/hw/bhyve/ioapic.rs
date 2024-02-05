// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::common::Lifecycle;
use crate::migrate::*;
use crate::vmm::VmmHdl;

/// Bhyve VMM-emulated IO-APIC (Intel 82093AA)
pub struct BhyveIoApic {
    hdl: Arc<VmmHdl>,
}
impl BhyveIoApic {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl Lifecycle for BhyveIoApic {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-ioapic"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for BhyveIoApic {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(migrate::IoApicV1::read(&self.hdl)?.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        offer.take::<migrate::IoApicV1>()?.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct IoApicV1 {
        pub id: u32,
        pub reg_sel: u32,
        pub registers: [u64; 32],
        pub levels: [u32; 32],
    }
    impl From<bhyve_api::vdi_ioapic_v1> for IoApicV1 {
        fn from(value: bhyve_api::vdi_ioapic_v1) -> Self {
            Self {
                id: value.vi_id,
                reg_sel: value.vi_reg_sel,
                registers: value.vi_pin_reg,
                levels: value.vi_pin_level,
            }
        }
    }
    impl Into<bhyve_api::vdi_ioapic_v1> for IoApicV1 {
        fn into(self) -> bhyve_api::vdi_ioapic_v1 {
            bhyve_api::vdi_ioapic_v1 {
                vi_pin_reg: self.registers,
                vi_pin_level: self.levels,
                vi_id: self.id,
                vi_reg_sel: self.reg_sel,
            }
        }
    }

    impl IoApicV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi = hdl
                .data_op(bhyve_api::VDC_IOAPIC, 1)
                .read::<bhyve_api::vdi_ioapic_v1>()?;

            Ok(vdi.into())
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            hdl.data_op(bhyve_api::VDC_IOAPIC, 1)
                .write::<bhyve_api::vdi_ioapic_v1>(&self.into())?;

            Ok(())
        }
    }
    impl Schema<'_> for IoApicV1 {
        fn id() -> SchemaId {
            ("bhyve-ioapic", 1)
        }
    }
}
