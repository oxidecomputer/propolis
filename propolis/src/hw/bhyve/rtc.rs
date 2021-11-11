use std::io::Result;
use std::sync::Arc;
use std::time::SystemTime;

use crate::dispatch::DispCtx;
use crate::inventory::Entity;
use crate::migrate::Migrate;
use crate::vmm::VmmHdl;

use erased_serde::Serialize;

const MEM_CHUNK: usize = 64 * 1024;
const MEM_BASE: usize = 16 * 1024 * 1024;

const NVRAM_OFF: u8 = 0xe;
const NVRAM_HIGH_OFF: u8 = 0x33;

const NVRAM_LEN: usize = 36;
const NVRAM_HIGH_LEN: usize = 77;

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
    pub fn set_time(&self, time: SystemTime, hdl: &VmmHdl) -> Result<()> {
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
    ) -> Result<()> {
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
    fn migrate(&self) -> Option<&dyn Migrate> {
        Some(self)
    }
}
impl Migrate for BhyveRtc {
    fn export(&self, ctx: &DispCtx) -> Box<dyn Serialize> {
        let hdl = ctx.mctx.hdl();
        let mut data = migrate::BhyveRtcV1::default();

        for (i, val) in data.nvram.iter_mut().enumerate() {
            *val = hdl.rtc_read(NVRAM_OFF + i as u8).unwrap();
        }
        for (i, val) in data.nvram_high.iter_mut().enumerate() {
            *val = hdl.rtc_read(NVRAM_HIGH_OFF + i as u8).unwrap();
        }
        // TODO: export rest of RTC data
        Box::new(data)
    }
}

pub mod migrate {
    use super::{NVRAM_HIGH_LEN, NVRAM_LEN};
    use serde::Serialize;

    #[derive(Serialize)]
    pub struct BhyveRtcV1 {
        #[serde(with = "serde_arrays")]
        pub nvram: [u8; NVRAM_LEN],
        #[serde(with = "serde_arrays")]
        pub nvram_high: [u8; NVRAM_HIGH_LEN],
    }
    impl Default for BhyveRtcV1 {
        fn default() -> Self {
            Self { nvram: [0; NVRAM_LEN], nvram_high: [0; NVRAM_HIGH_LEN] }
        }
    }
}
