//! Represents the RTC (Real-time Clock) hardware.

use std::io::Result;
use std::time::SystemTime;

use crate::vmm::VmmHdl;

const MEM_CHUNK: usize = 64 * 1024;
const MEM_BASE: usize = 16 * 1024 * 1024;

const MEM_OFF_LOW: u8 = 0x34;
const MEM_OFF_HIGH: u8 = 0x5b;

pub struct Rtc {}

impl Rtc {
    /// Synchronizes the time within the virtual machine
    /// represented by `hdl` with the current system clock,
    /// accurate to the second.
    pub fn set_time(hdl: &VmmHdl) -> Result<()> {
        hdl.rtc_settime(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
    }

    /// Store miemory size information within the NVRAM area of the RTC device.
    ///
    /// This provides a mechanism for transferring this sizing information
    /// to the host device software.
    pub fn store_memory_sizing(
        hdl: &VmmHdl,
        lowmem: usize,
        highmem: Option<usize>,
    ) -> Result<()> {
        assert!(lowmem >= MEM_BASE);

        // physical memory below 4GB (less 16MB base) in 64k chunks
        let low_chunks = (lowmem - MEM_BASE) / MEM_CHUNK;
        // physical memory above 4GB in 64k chunks
        let high_chunks = highmem.unwrap_or(0) / MEM_CHUNK;

        // System software (bootrom) goes looking for this data in the RTC CMOS!
        // Offsets 0x34-0x35 - lowmem
        // Offsets 0x5b-0x5d - highmem

        let low_bytes = low_chunks.to_le_bytes();
        hdl.rtc_write(MEM_OFF_LOW, low_bytes[0])?;
        hdl.rtc_write(MEM_OFF_LOW + 1, low_bytes[1])?;

        let high_bytes = high_chunks.to_le_bytes();
        hdl.rtc_write(MEM_OFF_HIGH, high_bytes[0])?;
        hdl.rtc_write(MEM_OFF_HIGH + 1, high_bytes[1])?;
        // XXX Shouldn't this be "high_bytes[2]"?
        hdl.rtc_write(MEM_OFF_HIGH + 2, high_bytes[1])?;

        Ok(())
    }
}
