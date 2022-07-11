use std::io;
use std::sync::Arc;
use std::time::SystemTime;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, MigrateStateError, Migrator};
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

pub struct BhyveRtc {}
impl BhyveRtc {
    pub fn create() -> Arc<Self> {
        Arc::new(Self {})
    }

    /// Synchronizes the time within the virtual machine
    /// represented by `hdl` with the current system clock,
    /// accurate to the second.
    pub fn set_time(&self, time: SystemTime, hdl: &VmmHdl) -> io::Result<()> {
        hdl.rtc_settime(
            time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
        )
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
        hdl: &VmmHdl,
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
        hdl.rtc_write(CMOS_OFF_MEM_BASE, base[0])?;
        hdl.rtc_write(CMOS_OFF_MEM_BASE + 1, base[1])?;

        // Next 64MiB
        if low_mem > 1 * MIB {
            let ext = (((low_mem - (1 * MIB)) / KIB) as u16).to_le_bytes();

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

impl Entity for BhyveRtc {
    fn type_name(&self) -> &'static str {
        "lpc-bhyve-rtc"
    }
    fn migrate(&self) -> Migrator {
        Migrator::Custom(self)
    }
}
impl Migrate for BhyveRtc {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        Box::new(migrate::BhyveRtcV1::read(hdl))
    }

    fn import(
        &self,
        _dev: &str,
        deserializer: &mut dyn erased_serde::Deserializer,
        ctx: &DispCtx,
    ) -> Result<(), MigrateStateError> {
        let deserialized: migrate::BhyveRtcV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(ctx.mctx.hdl())?;
        Ok(())
    }
}

pub mod migrate {
    use crate::vmm;

    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub struct BhyveRtcV1 {
        #[serde(with = "serde_arrays")]
        pub cmos: [u8; 128],
        pub time_sec: u64,
        pub time_nsec: u64,
        pub time_base: u64,
        pub addr: u8,
    }

    impl BhyveRtcV1 {
        pub(super) fn read(hdl: &vmm::VmmHdl) -> Self {
            let vdi: bhyve_api::vdi_rtc_v1 =
                vmm::data::read(hdl, -1, bhyve_api::VDC_RTC, 1).unwrap();
            Self {
                cmos: vdi.vr_content,
                time_sec: vdi.vr_rtc_sec,
                time_nsec: vdi.vr_rtc_nsec,
                time_base: vdi.vr_time_base,
                addr: vdi.vr_addr,
            }
        }

        pub(super) fn write(self, hdl: &vmm::VmmHdl) -> std::io::Result<()> {
            let vdi = bhyve_api::vdi_rtc_v1 {
                vr_content: self.cmos,
                vr_addr: self.addr,
                vr_time_base: self.time_base,
                vr_rtc_sec: self.time_sec,
                vr_rtc_nsec: self.time_nsec,
            };
            vmm::data::write(hdl, -1, bhyve_api::VDC_RTC, 1, vdi)?;
            Ok(())
        }
    }
}
