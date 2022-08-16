use std::sync::Arc;

use crate::inventory::Entity;
use crate::migrate::*;
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

pub struct BhyveIoApic {
    hdl: Arc<VmmHdl>,
}
impl BhyveIoApic {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
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
    fn export(&self, _ctx: &MigrateCtx) -> Box<dyn Serialize> {
        Box::new(migrate::BhyveIoApicV1::read(&self.hdl))
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let deserialized: migrate::BhyveIoApicV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
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

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            let vdi = bhyve_api::vdi_ioapic_v1 {
                vi_pin_reg: self.registers,
                vi_pin_level: self.levels,
                vi_id: self.id,
                vi_reg_sel: self.reg_sel,
            };
            vmm::data::write(hdl, -1, bhyve_api::VDC_IOAPIC, 1, vdi)?;
            Ok(())
        }
    }
}
