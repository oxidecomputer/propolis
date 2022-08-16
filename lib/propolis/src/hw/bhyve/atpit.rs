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
        Box::new(migrate::BhyveAtPitV1::read(&self.hdl))
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let deserialized: migrate::BhyveAtPitV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct BhyveAtPitV1 {
        /// XXX: do not expose vdi struct
        pub data: bhyve_api::vdi_atpit_v1,
    }

    impl BhyveAtPitV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            Self {
                data: vmm::data::read(hdl, -1, bhyve_api::VDC_ATPIT, 1)
                    .unwrap(),
            }
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            vmm::data::write(hdl, -1, bhyve_api::VDC_ATPIT, 1, self.data)?;
            Ok(())
        }
    }
}
