// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use crate::common::Lifecycle;
use crate::migrate::*;
use crate::vmm::VmmHdl;

/// Bhyve VMM-emulated PIT (Intel 8254)
pub struct BhyveAtPit {
    hdl: Arc<VmmHdl>,
}
impl BhyveAtPit {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl Lifecycle for BhyveAtPit {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-atpit"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for BhyveAtPit {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(migrate::AtPitV1::read(&self.hdl)?.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        offer.take::<migrate::AtPitV1>()?.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Copy, Clone, Default, Deserialize, Serialize)]
    pub struct AtPitV1 {
        pub channel: [AtPitChannelV1; 3],
    }
    impl From<bhyve_api::vdi_atpit_v1> for AtPitV1 {
        fn from(value: bhyve_api::vdi_atpit_v1) -> Self {
            Self {
                channel: [
                    value.va_channel[0].into(),
                    value.va_channel[1].into(),
                    value.va_channel[2].into(),
                ],
            }
        }
    }
    impl Into<bhyve_api::vdi_atpit_v1> for AtPitV1 {
        fn into(self) -> bhyve_api::vdi_atpit_v1 {
            bhyve_api::vdi_atpit_v1 {
                va_channel: [
                    self.channel[0].into(),
                    self.channel[1].into(),
                    self.channel[2].into(),
                ],
            }
        }
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
    impl From<bhyve_api::vdi_atpit_channel_v1> for AtPitChannelV1 {
        fn from(value: bhyve_api::vdi_atpit_channel_v1) -> Self {
            Self {
                initial: value.vac_initial,
                reg_cr: value.vac_reg_cr,
                reg_ol: value.vac_reg_ol,
                reg_status: value.vac_reg_status,
                mode: value.vac_mode,
                status: value.vac_status,
                time_target: value.vac_time_target,
            }
        }
    }
    impl Into<bhyve_api::vdi_atpit_channel_v1> for AtPitChannelV1 {
        fn into(self) -> bhyve_api::vdi_atpit_channel_v1 {
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

    impl AtPitV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi = hdl
                .data_op(bhyve_api::VDC_ATPIT, 1)
                .read::<bhyve_api::vdi_atpit_v1>()?;

            Ok(vdi.into())
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            hdl.data_op(bhyve_api::VDC_ATPIT, 1)
                .write::<bhyve_api::vdi_atpit_v1>(&self.into())?;

            Ok(())
        }
    }
    impl Schema<'_> for AtPitV1 {
        fn id() -> SchemaId {
            ("bhyve-atpit", 1)
        }
    }
}
