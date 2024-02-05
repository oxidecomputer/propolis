// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use crate::common::Lifecycle;
use crate::migrate::*;
use crate::vmm::VmmHdl;

/// Bhyve VMM-emulated ACPI PM timer (Intel PIIX3/4-ish)
pub struct BhyvePmTimer {
    hdl: Arc<VmmHdl>,
    inner: Mutex<Inner>,
}
struct Inner {
    ioport: u16,
    attached_ioport: Option<u16>,
}
impl BhyvePmTimer {
    pub fn create(hdl: Arc<VmmHdl>, ioport: u16) -> Arc<Self> {
        Arc::new(Self {
            hdl,
            inner: Mutex::new(Inner { ioport, attached_ioport: None }),
        })
    }
    fn update_attachment(&self) {
        let mut inner = self.inner.lock().unwrap();

        let target = inner.ioport;
        let exists = inner.attached_ioport.as_ref();
        if matches!(exists, Some(p) if *p == target) {
            // Attachment is already correct
            return;
        }

        match self.hdl.pmtmr_locate(target) {
            Ok(()) => {
                inner.attached_ioport = Some(target);
            }
            Err(_e) => {
                inner.attached_ioport = None;
                // TODO: squawk about it?
            }
        }
    }
}

impl Lifecycle for BhyvePmTimer {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-pmtimer"
    }
    fn reset(&self) {
        // When the instance is reset, in-kernel attachment of the PM timer IO
        // port may change.
        //
        // We clear `atttached_ioport` here so that a subsequent call of
        // `update_attachement()` during instance start will force the in-kernel
        // configuration of the port to match expectations.
        self.inner.lock().unwrap().attached_ioport = None;
    }
    fn resume(&self) {
        // If the machine was reset
        self.update_attachment();
    }
    fn start(&self) -> anyhow::Result<()> {
        self.update_attachment();
        Ok(())
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for BhyvePmTimer {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(migrate::PmTimerV1::read(&self.hdl)?.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        let data: migrate::PmTimerV1 = offer.take()?;

        data.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Default, Deserialize, Serialize)]
    pub struct PmTimerV1 {
        pub start_time: i64,
    }
    impl PmTimerV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi = hdl
                .data_op(bhyve_api::VDC_PM_TIMER, 1)
                .read::<bhyve_api::vdi_pm_timer_v1>()?;

            Ok(Self {
                // vdi_pm_timer_v1 also carries the ioport to which the pmtimer
                // is attached, but migration of that state is handled by the
                // chipset PM device.
                start_time: vdi.vpt_time_base,
            })
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            let vdi = bhyve_api::vdi_pm_timer_v1 {
                vpt_time_base: self.start_time,
                // The IO-port field is ignored for writes
                vpt_ioport: 0,
            };
            hdl.data_op(bhyve_api::VDC_PM_TIMER, 1).write(&vdi)?;
            Ok(())
        }
    }
    impl Schema<'_> for PmTimerV1 {
        fn id() -> SchemaId {
            ("bhyve-pmtimer", 1)
        }
    }
}
