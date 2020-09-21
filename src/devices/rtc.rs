use std::io::Result;
use std::os::raw::c_void;
use std::time::SystemTime;

use crate::vm::{VmCtx, VmHdl};
use bhyve_api;

const MEM_CHUNK: usize = 64 * 1024;
const MEM_BASE: usize = 16 * 1024 * 1024;

const MEM_OFF_LOW: u8 = 0x34;
const MEM_OFF_HIGH: u8 = 0x5b;

pub struct Rtc {}

impl Rtc {
    pub fn set_time(ctx: &VmCtx) -> Result<()> {
        let mut time: u64 = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        ctx.borrow_hdl()
            .ioctl(bhyve_api::VM_RTC_SETTIME, &mut time)?;
        Ok(())
    }

    pub fn store_memory_sizing(ctx: &VmCtx, lowmem: usize, highmem: Option<usize>) -> Result<()> {
        assert!(lowmem >= MEM_BASE);

        // physical memory below 4GB (less 16MB base) in 64k chunks
        let low_chunks = (lowmem - MEM_BASE) / MEM_CHUNK;
        // physical memory above 4GB in 64k chunks
        let high_chunks = highmem.unwrap_or(0) / MEM_CHUNK;

        // System software (bootrom) goes looking for this data in the RTC CMOS!
        // Offsets 0x34-0x35 - lowmem
        // Offsets 0x5b-0x5d - highmem
        let hdl = ctx.borrow_hdl();

        let low_bytes = low_chunks.to_le_bytes();
        Self::cmos_write(hdl, MEM_OFF_LOW, low_bytes[0])?;
        Self::cmos_write(hdl, MEM_OFF_LOW + 1, low_bytes[1])?;

        let high_bytes = high_chunks.to_le_bytes();
        Self::cmos_write(hdl, MEM_OFF_HIGH, high_bytes[0])?;
        Self::cmos_write(hdl, MEM_OFF_HIGH + 1, high_bytes[1])?;
        Self::cmos_write(hdl, MEM_OFF_HIGH + 2, high_bytes[1])?;

        Ok(())
    }
    fn cmos_write(hdl: &VmHdl, offset: u8, value: u8) -> Result<()> {
        let mut data = bhyve_api::vm_rtc_data {
            offset: offset as i32,
            value,
        };
        hdl.ioctl(bhyve_api::VM_RTC_WRITE, &mut data)?;
        Ok(())
    }
}
