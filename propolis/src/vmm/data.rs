//! Interface around the VMM data operations

use std::io;
use std::mem::size_of;

use super::hdl::VmmHdl;

use bhyve_api::vm_data_xfer;
use libc::c_void;

#[derive(Debug)]
pub enum VmmDataError {
    IoError(io::Error),
    SpaceNeeded(u32),
}

impl From<VmmDataError> for std::io::Error {
    fn from(err: VmmDataError) -> Self {
        use std::io::{Error, ErrorKind};
        match err {
            VmmDataError::IoError(e) => e,
            VmmDataError::SpaceNeeded(c) => {
                // ErrorKind::StorageFull would more accurately match the underlying ENOSPC
                // but that variant is unstable still
                Error::new(
                    ErrorKind::Other,
                    format!("operation requires {} bytes", c),
                )
            }
        }
    }
}

fn ioctl_xlate(
    hdl: &VmmHdl,
    op: i32,
    xfer: &mut bhyve_api::vm_data_xfer,
) -> Result<u32, VmmDataError> {
    match hdl.ioctl(op, xfer) {
        Err(e) => match e.raw_os_error() {
            Some(errno) if errno == libc::ENOSPC => {
                Err(VmmDataError::SpaceNeeded(xfer.vdx_result_len))
            }
            _ => Err(VmmDataError::IoError(e)),
        },
        Ok(_) => Ok(xfer.vdx_result_len),
    }
}

pub fn read<T: Sized + Copy + Default>(
    hdl: &VmmHdl,
    vcpuid: i32,
    class: u16,
    version: u16,
) -> Result<T, VmmDataError> {
    let mut data: T = Default::default();
    let mut xfer = vm_data_xfer {
        vdx_vcpuid: vcpuid,
        vdx_class: class,
        vdx_version: version,
        vdx_flags: 0,
        vdx_len: std::mem::size_of::<T>() as u32,
        vdx_result_len: 0,
        vdx_data: &mut data as *mut T as *mut c_void,
    };
    let bytes_read = ioctl_xlate(hdl, bhyve_api::VM_DATA_READ, &mut xfer)?;
    assert_eq!(bytes_read, size_of::<T>() as u32);
    Ok(data)
}
pub fn read_many<T: Sized + Copy>(
    hdl: &VmmHdl,
    vcpuid: i32,
    class: u16,
    version: u16,
) -> Result<Vec<T>, VmmDataError> {
    let mut xfer = vm_data_xfer {
        vdx_vcpuid: vcpuid,
        vdx_class: class,
        vdx_version: version,
        vdx_flags: 0,
        vdx_len: 0,
        vdx_result_len: 0,
        vdx_data: std::ptr::null_mut(),
    };
    let sz = match ioctl_xlate(hdl, bhyve_api::VM_DATA_READ, &mut xfer) {
        Err(VmmDataError::SpaceNeeded(sz)) => Ok(sz),
        Err(e) => Err(e),
        Ok(_) => panic!("unexpected success"),
    }?;
    let item_sz = size_of::<T>() as u32;
    assert!(sz >= item_sz);
    let item_count = sz / item_sz;
    let mut data: Vec<T> = Vec::with_capacity(item_count as usize);
    xfer.vdx_len = item_count * item_sz;
    xfer.vdx_data = data.as_mut_ptr() as *mut c_void;
    let bytes_read = ioctl_xlate(hdl, bhyve_api::VM_DATA_READ, &mut xfer)?;

    assert!(bytes_read <= item_count * item_sz);
    // SAFETY: Data is populated by the ioctl
    unsafe {
        data.set_len(item_count as usize);
    }

    Ok(data)
}

pub fn write<T: Sized + Copy>(
    hdl: &VmmHdl,
    vcpuid: i32,
    class: u16,
    version: u16,
    mut data: T,
) -> Result<(), VmmDataError> {
    let mut xfer = vm_data_xfer {
        vdx_vcpuid: vcpuid,
        vdx_class: class,
        vdx_version: version,
        vdx_flags: 0,
        vdx_len: std::mem::size_of::<T>() as u32,
        vdx_result_len: 0,
        vdx_data: &mut data as *mut T as *mut c_void,
    };
    let bytes_written = ioctl_xlate(hdl, bhyve_api::VM_DATA_WRITE, &mut xfer)?;
    assert_eq!(bytes_written, size_of::<T>() as u32);
    Ok(())
}

pub fn write_many<T: Sized>(
    hdl: &VmmHdl,
    vcpuid: i32,
    class: u16,
    version: u16,
    data: &mut [T],
) -> Result<(), VmmDataError> {
    let write_len = (std::mem::size_of::<T>() * data.len()) as u32;
    let mut xfer = vm_data_xfer {
        vdx_vcpuid: vcpuid,
        vdx_class: class,
        vdx_version: version,
        vdx_flags: 0,
        vdx_len: write_len,
        vdx_result_len: 0,
        vdx_data: data.as_mut_ptr() as *mut c_void,
    };
    let bytes_written = ioctl_xlate(hdl, bhyve_api::VM_DATA_WRITE, &mut xfer)?;
    assert_eq!(bytes_written, write_len);
    Ok(())
}
