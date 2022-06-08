use std::sync::Arc;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, MigrateStateError, Migrator};

use erased_serde::Serialize;

pub struct BhyveHpet {}
impl BhyveHpet {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }
}

impl Entity for BhyveHpet {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-hpet"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for BhyveHpet {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        Box::new(migrate::BhyveHpetV1::read(hdl))
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        _ctx: &DispCtx,
    ) -> Result<(), MigrateStateError> {
        // TODO: import deserialized state
        let _deserialized: migrate::BhyveHpetV1 =
            erased_serde::deserialize(deserializer)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Deserialize, Serialize)]
    pub struct BhyveHpetV1 {
        /// XXX: do not expose vdi struct
        pub data: bhyve_api::vdi_hpet_v1,
    }

    impl BhyveHpetV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            Self {
                data: vmm::data::read(hdl, -1, bhyve_api::VDC_HPET, 1).unwrap(),
            }
        }
    }
}
