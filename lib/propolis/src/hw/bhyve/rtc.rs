// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::common::Lifecycle;
use crate::migrate::*;
use crate::vmm::VmmHdl;

/// Bhyve VMM-emulated RTC (MC146818 or similar)
pub struct BhyveRtc {
    hdl: Arc<VmmHdl>,
}
impl BhyveRtc {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }

    /// Synchronizes the time within the virtual machine
    /// represented by `hdl` with the current system clock,
    /// accurate to the second.
    pub fn set_time(&self, time: Duration) -> io::Result<()> {
        self.hdl.rtc_settime(time)
    }

    /// Store memory size information within the NVRAM area of the RTC device.
    ///
    /// This provides a mechanism for transferring this sizing information
    /// to the host device software.
    /// - `low_mem_bytes`: Memory below 32-bit boundary, must be != 0
    /// - `high_mem_bytes`: Memory above 32-bit boundary
    ///
    /// Size(s) must be aligned to 4KiB.
    pub fn memsize_to_nvram(
        &self,
        low_mem_bytes: u32,
        high_mem_bytes: u64,
    ) -> io::Result<()> {
        assert_ne!(low_mem_bytes, 0, "low-mem must not be zero");
        assert_eq!(low_mem_bytes & 0xfff, 0, "low-mem must be 4KiB aligned");
        assert_eq!(high_mem_bytes & 0xfff, 0, "high-mem must be 4KiB aligned");

        // We mimic the CMOS layout of qemu (expected by OVMF) when it comes to
        // communicating the sizing of instance memory:
        //
        // - 0x15-0x16: Base memory in KiB (0-1MiB, less 384KiB BDA)
        // - 0x17-0x18: Extended memory in KiB (1MiB-64MiB)
        // - 0x30-0x31: Extended memory (duplicate)
        // - 0x34-0x35: Low-mem, less 16MiB, in 64KiB units
        // - 0x5b-0x5d: High-mem in 64KiB units

        const CMOS_OFF_MEM_BASE: u8 = 0x15;
        const CMOS_OFF_MEM_EXT: u8 = 0x17;
        const CMOS_OFF_MEM_EXT_DUP: u8 = 0x30;
        const CMOS_OFF_MEM_LOW: u8 = 0x34;
        const CMOS_OFF_MEM_HIGH: u8 = 0x5b;

        const KIB: usize = 1024;
        const MIB: usize = 1024 * 1024;
        const CHUNK: usize = 64 * KIB;

        // Convert for convenience
        let low_mem = low_mem_bytes as usize;
        let high_mem = high_mem_bytes as usize;

        // First 1MiB, less 384KiB
        let base = u16::min((low_mem / KIB) as u16, 640).to_le_bytes();
        let hdl = &self.hdl;
        hdl.rtc_write(CMOS_OFF_MEM_BASE, base[0])?;
        hdl.rtc_write(CMOS_OFF_MEM_BASE + 1, base[1])?;

        // Next 64MiB
        if low_mem > MIB {
            let ext = (((low_mem - MIB) / KIB) as u16).to_le_bytes();

            hdl.rtc_write(CMOS_OFF_MEM_EXT, ext[0])?;
            hdl.rtc_write(CMOS_OFF_MEM_EXT + 1, ext[1])?;

            // ... and in the duplicate location
            hdl.rtc_write(CMOS_OFF_MEM_EXT_DUP, ext[0])?;
            hdl.rtc_write(CMOS_OFF_MEM_EXT_DUP + 1, ext[1])?;
        }

        // Low-mem, less 16MiB
        if low_mem > 16 * MIB {
            let low = (((low_mem - 16 * MIB) / CHUNK) as u16).to_le_bytes();

            hdl.rtc_write(CMOS_OFF_MEM_LOW, low[0])?;
            hdl.rtc_write(CMOS_OFF_MEM_LOW + 1, low[1])?;
        }

        // High-mem
        if high_mem > 0 {
            let high = ((high_mem / CHUNK) as u32).to_le_bytes();

            hdl.rtc_write(CMOS_OFF_MEM_HIGH, high[0])?;
            hdl.rtc_write(CMOS_OFF_MEM_HIGH + 1, high[1])?;
            hdl.rtc_write(CMOS_OFF_MEM_HIGH + 2, high[2])?;
        }

        Ok(())
    }
}

impl Lifecycle for BhyveRtc {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-rtc"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Multi(self)
    }
}
impl MigrateMulti for BhyveRtc {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        output.push(migrate::BhyveRtcV2::read(&self.hdl)?.into())
    }

    fn import(
        &self,
        offer: &mut PayloadOffers,
        _ctx: &MigrateCtx,
    ) -> Result<(), MigrateStateError> {
        offer.take::<migrate::BhyveRtcV2>()?.write(&self.hdl)?;
        Ok(())
    }
}

pub mod migrate {
    use crate::migrate::*;
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct BhyveRtcV2 {
        pub base_clock: i64,
        pub last_period: i64,
        #[serde(with = "serde_arrays")]
        pub cmos: [u8; 128],
        pub addr: u8,
    }
    impl From<bhyve_api::vdi_rtc_v2> for BhyveRtcV2 {
        fn from(value: bhyve_api::vdi_rtc_v2) -> Self {
            Self {
                base_clock: value.vr_base_clock,
                last_period: value.vr_last_period,
                cmos: value.vr_content,
                addr: value.vr_addr,
            }
        }
    }
    impl Into<bhyve_api::vdi_rtc_v2> for BhyveRtcV2 {
        fn into(self) -> bhyve_api::vdi_rtc_v2 {
            bhyve_api::vdi_rtc_v2 {
                vr_base_clock: self.base_clock,
                vr_last_period: self.last_period,
                vr_content: self.cmos,
                vr_addr: self.addr,
            }
        }
    }

    impl BhyveRtcV2 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> std::io::Result<Self> {
            let vdi = hdl
                .data_op(bhyve_api::VDC_RTC, 2)
                .read::<bhyve_api::vdi_rtc_v2>()?;

            Ok(vdi.into())
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            hdl.data_op(bhyve_api::VDC_RTC, 2)
                .write::<bhyve_api::vdi_rtc_v2>(&self.into())?;

            Ok(())
        }
    }
    impl Schema<'_> for BhyveRtcV2 {
        fn id() -> SchemaId {
            ("bhyve-rtc", 2)
        }
    }
}
