use std::io;
use std::sync::Arc;
use std::time::SystemTime;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::{Migrate, MigrateStateError, Migrator};
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

const MEM_CHUNK: usize = 64 * 1024;
const MEM_BASE: usize = 16 * 1024 * 1024;

const MEM_OFF_LOW: u8 = 0x34;
const MEM_OFF_HIGH: u8 = 0x5b;

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
    pub fn memsize_to_nvram(
        &self,
        lowmem: usize,
        highmem: usize,
        hdl: &VmmHdl,
    ) -> io::Result<()> {
        assert!(lowmem >= MEM_BASE);

        // physical memory below 4GB (less 16MB base) in 64k chunks
        let low_chunks = (lowmem - MEM_BASE) / MEM_CHUNK;
        // physical memory above 4GB in 64k chunks
        let high_chunks = highmem / MEM_CHUNK;

        // System software (bootrom) goes looking for this data in the RTC CMOS!
        // Offsets 0x34-0x35 - lowmem
        // Offsets 0x5b-0x5d - highmem

        let low_bytes = low_chunks.to_le_bytes();
        hdl.rtc_write(MEM_OFF_LOW, low_bytes[0])?;
        hdl.rtc_write(MEM_OFF_LOW + 1, low_bytes[1])?;

        let high_bytes = high_chunks.to_le_bytes();
        hdl.rtc_write(MEM_OFF_HIGH, high_bytes[0])?;
        hdl.rtc_write(MEM_OFF_HIGH + 1, high_bytes[1])?;
        hdl.rtc_write(MEM_OFF_HIGH + 2, high_bytes[2])?;

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
        _ctx: &DispCtx,
    ) -> Result<(), MigrateStateError> {
        // TODO: import deserialized state
        let _deserialized: migrate::BhyveRtcV1 =
            erased_serde::deserialize(deserializer)?;
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
    }
}
