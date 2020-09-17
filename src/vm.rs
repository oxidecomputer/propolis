use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::ptr;

use bhyve_api;
use libc;

pub fn create_vm(name: &str) -> Result<VmHdl> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let namestr = CString::new(name).or_else(|_x| Err(Error::from_raw_os_error(libc::EINVAL)))?;
    let nameptr = namestr.as_ptr();
    let ctlfd = ctl.as_raw_fd();

    let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, nameptr) };
    if res != 0 {
        let err = Error::last_os_error();
        if err.kind() != ErrorKind::AlreadyExists {
            return Err(err);
        }
        // try to nuke(!) the existing vm
        let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_DESTROY_VM, nameptr) };
        if res != 0 {
            let err = Error::last_os_error();
            if err.kind() != ErrorKind::NotFound {
                return Err(err);
            }
        }
        // attempt to create in its presumed absence
        let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, nameptr) };
        if res != 0 {
            return Err(Error::last_os_error());
        }
    }

    let mut vmpath = PathBuf::from(bhyve_api::VMM_PATH_PREFIX);
    vmpath.push(name);

    let fp = OpenOptions::new().write(true).read(true).open(vmpath)?;
    Ok(VmHdl { fp })
}

#[repr(u8)]
#[allow(non_camel_case_types)]
enum VmMemsegs {
    SEG_LOWMEM,
    SEG_BOOTROM,
}

pub struct VmHdl {
    fp: File,
}

fn vm_ioctl<T>(fp: &File, cmd: i32, data: *mut T) -> Result<i32> {
    let fd = fp.as_raw_fd();
    let res = unsafe { libc::ioctl(fd, cmd, data) };
    if res == -1 {
        Err(Error::last_os_error())
    } else {
        Ok(res)
    }
}

impl VmHdl {
    pub fn setup_memory(&mut self, size: u64) -> Result<()> {
        let segid = VmMemsegs::SEG_LOWMEM as i32;
        let mut seg = bhyve_api::vm_memseg {
            segid,
            len: size as usize,
            name: [0u8; bhyve_api::SEG_NAME_LEN],
        };
        vm_ioctl(&self.fp, bhyve_api::VM_ALLOC_MEMSEG, &mut seg)?;
        self.map_memseg(segid, 0, size as usize, 0, bhyve_api::PROT_ALL)
    }

    pub fn setup_bootrom(&mut self, len: usize) -> Result<()> {
        let segid = VmMemsegs::SEG_BOOTROM as i32;
        let mut seg = bhyve_api::vm_memseg {
            segid,
            len,
            name: [0u8; bhyve_api::SEG_NAME_LEN],
        };

        let mut name = &mut seg.name[..];
        name.write("bootrom".as_bytes())?;
        vm_ioctl(&self.fp, bhyve_api::VM_ALLOC_MEMSEG, &mut seg)?;

        // map the bootrom so the first instruction lines up at 0xfffffff0
        let gpa = 0x1_0000_0000 - len as u64;
        self.map_memseg(
            segid,
            gpa,
            len,
            0,
            bhyve_api::PROT_READ | bhyve_api::PROT_EXEC,
        )?;
        Ok(())
    }

    pub fn populate_bootrom(&mut self, input: &mut File, len: usize) -> Result<()> {
        let mut devoff = bhyve_api::vm_devmem_offset {
            segid: VmMemsegs::SEG_BOOTROM as i32,
            offset: 0,
        };
        // find the devmem offset
        vm_ioctl(&self.fp, bhyve_api::VM_DEVMEM_GETOFFSET, &mut devoff)?;
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                0,
                self.fp.as_raw_fd(),
                devoff.offset,
            )
        };
        if ptr.is_null() {
            return Err(Error::last_os_error());
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };

        let res = match input.read(buf) {
            Ok(n) if n == len => Ok(()),
            Ok(_) => {
                // TODO: handle short read
                Ok(())
            }
            Err(e) => Err(e),
        };

        unsafe {
            libc::munmap(ptr, len as usize);
        }

        res
    }

    pub fn vcpu(&self, id: i32) -> VcpuHdl {
        assert!(id >= 0 && id < bhyve_api::VM_MAXCPU as i32);
        let fp = unsafe {
            // just cheat and clone via the raw fd for now
            File::from_raw_fd(self.fp.as_raw_fd())
        };
        VcpuHdl { fp, id }
    }

    fn map_memseg(&mut self, id: i32, gpa: u64, len: usize, off: u64, prot: u8) -> Result<()> {
        assert!(off <= i64::MAX as u64);
        let mut map = bhyve_api::vm_memmap {
            gpa,
            segid: id,
            segoff: off as i64,
            len,
            prot: prot as i32,
            flags: 0,
        };
        vm_ioctl(&self.fp, bhyve_api::VM_MMAP_MEMSEG, &mut map)?;
        Ok(())
    }
}

pub struct VcpuHdl {
    fp: File,
    id: i32,
}

impl VcpuHdl {
    pub fn set_reg(&mut self, reg: bhyve_api::vm_reg_name, val: u64) -> Result<()> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: val,
        };

        vm_ioctl(&self.fp, bhyve_api::VM_MMAP_MEMSEG, &mut regcmd)?;
        Ok(())
    }
    pub fn get_reg(&mut self, reg: bhyve_api::vm_reg_name) -> Result<u64> {
        let mut regcmd = bhyve_api::vm_register {
            cpuid: self.id,
            regnum: reg as i32,
            regval: 0,
        };

        vm_ioctl(&self.fp, bhyve_api::VM_MMAP_MEMSEG, &mut regcmd)?;
        Ok(regcmd.regval)
    }
}
