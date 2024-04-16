// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Module responsible for communicating with the kernel's VMM.
//!
//! Responsible for both issuing commands to the bhyve
//! kernel controller to create and destroy VMs.
//!
//! Additionally, contains a wrapper struct ([`VmmHdl`])
//! for encapsulating commands to the underlying kernel
//! object which represents a single VM.

use std::fs::File;
use std::io::{Error, ErrorKind, Result, Write};
use std::os::raw::c_void;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::common::PAGE_SIZE;
use crate::vmm::mem::Prot;

/// Configurable options for VMM instance creation
///
/// # Options:
/// - `force`: If a VM with the name `name` already exists, attempt
///   to destroy the VM before creating it.
/// - `use_reservoir`: Allocate guest memory (only) from the VMM reservoir.  If
/// this is enabled, and memory in excess of what is available from the
/// reservoir is requested, creation of that guest memory resource will fail.
#[derive(Default, Copy, Clone)]
pub struct CreateOpts {
    pub force: bool,
    pub use_reservoir: bool,
    pub track_dirty: bool,
}

/// Creates a new virtual machine with the provided `name`.
///
/// Operates on the bhyve controller object at `/dev/vmmctl`,
/// which acts as an interface to the kernel module, and opens
/// an object at `/dev/vmm/{name}`.
///
/// # Arguments
/// - `name`: The name of the VM to create.
/// - `opts`: Creation options (detailed in `CreateOpts`)
pub(crate) fn create_vm(name: &str, opts: CreateOpts) -> Result<VmmHdl> {
    let ctl = bhyve_api::VmmCtlFd::open()?;

    let mut req = bhyve_api::vm_create_req::new(name.as_bytes())?;
    if opts.use_reservoir {
        req.flags |= bhyve_api::VCF_RESERVOIR_MEM;
    }
    if opts.track_dirty {
        req.flags |= bhyve_api::VCF_TRACK_DIRTY;
    }
    let res = unsafe { ctl.ioctl(bhyve_api::VMM_CREATE_VM, &mut req) };
    if let Err(e) = res {
        if e.kind() != ErrorKind::AlreadyExists || !opts.force {
            return Err(e);
        }

        // try to nuke(!) the existing vm
        ctl.vm_destroy(name.as_bytes()).or_else(|e| match e.kind() {
            ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })?;

        // now attempt to create in its presumed absence
        let _ = unsafe { ctl.ioctl(bhyve_api::VMM_CREATE_VM, &mut req) }?;
    }

    // Safety: Files opened within VMM_PATH_PREFIX are VMMs, which may not be
    // truncated.
    let inner = bhyve_api::VmmFd::open(name)?;

    Ok(VmmHdl {
        inner,
        destroyed: AtomicBool::new(false),
        name: name.to_string(),
        #[cfg(test)]
        is_test_hdl: false,
    })
}

/// A wrapper around a file which must uphold the guarantee that the underlying
/// structure may not be truncated.
pub struct VmmFile(File);

impl VmmFile {
    /// Constructs a new `VmmFile`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the provided file cannot be truncated.
    pub unsafe fn new(f: File) -> Self {
        VmmFile(f)
    }

    /// Accesses the VMM as a raw fd.
    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// A handle to an existing virtual machine monitor.
pub struct VmmHdl {
    pub(super) inner: bhyve_api::VmmFd,
    destroyed: AtomicBool,
    name: String,

    #[cfg(test)]
    /// Track if this VmmHdl belongs to a wholly fictitious Instance/Machine.
    is_test_hdl: bool,
}
impl VmmHdl {
    /// Accesses the raw file descriptor behind the VMM.
    pub fn fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
    /// Sends an ioctl to the underlying VMM.
    pub unsafe fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<()> {
        if self.destroyed.load(Ordering::Acquire) {
            return Err(Error::new(ErrorKind::NotFound, "instance destroyed"));
        }

        #[cfg(test)]
        if self.is_test_hdl {
            // Lie about all ioctl results, since there is no real vmm resource
            // underlying this handle.
            return Ok(());
        }

        self.inner.ioctl(cmd, data)?;
        Ok(())
    }

    /// Sends an ioctl (with usize param) to the underlying VMM.
    pub fn ioctl_usize(&self, cmd: i32, data: usize) -> Result<()> {
        if self.destroyed.load(Ordering::Acquire) {
            return Err(Error::new(ErrorKind::NotFound, "instance destroyed"));
        }

        #[cfg(test)]
        if self.is_test_hdl {
            // Lie about all ioctl results, since there is no real vmm resource
            // underlying this handle.
            return Ok(());
        }

        self.inner.ioctl_usize(cmd, data)?;
        Ok(())
    }

    /// Query the API version exposed by the kernel VMM.
    pub fn api_version(&self) -> Result<u32> {
        self.inner.api_version()
    }

    /// Allocate a memory segment within the VM.
    ///
    /// # Arguments
    /// - `segid`: The segment ID of the requested memory.
    /// - `size`: The size of the memory region, in bytes.
    /// - `segname`: The (optional) name of the memory segment.
    pub fn create_memseg(
        &self,
        segid: i32,
        size: usize,
        segname: Option<&str>,
    ) -> Result<()> {
        let mut seg = bhyve_api::vm_memseg {
            segid,
            len: size,
            name: [0u8; bhyve_api::VM_MAX_SEG_NAMELEN],
        };
        if let Some(name) = segname {
            let name_raw = name.as_bytes();

            assert!(name_raw.len() < bhyve_api::VM_MAX_SEG_NAMELEN);
            (&mut seg.name[..]).write_all(name_raw)?;
        }
        unsafe { self.ioctl(bhyve_api::VM_ALLOC_MEMSEG, &mut seg) }
    }

    /// Maps a memory segment within the guest address space.
    ///
    /// # Arguments
    /// - `segid`: The segment ID to be mapped.
    /// - `gpa`: The "Guest Physical Address" to be mapped.
    /// - `len`: The length of the mapping, in bytes. Must be page aligned.
    /// - `segoff`: Offset within the `gpa` where the mapping should occur.
    /// Must be page aligned.
    /// - `prot`: Memory protections to apply to the guest mapping.
    pub fn map_memseg(
        &self,
        segid: i32,
        gpa: usize,
        len: usize,
        segoff: usize,
        prot: Prot,
    ) -> Result<()> {
        assert!(segoff <= i64::MAX as usize);

        let mut map = bhyve_api::vm_memmap {
            gpa: gpa as u64,
            segid,
            segoff: segoff as i64,
            len,
            prot: prot.bits() as i32,
            flags: 0,
        };
        unsafe { self.ioctl(bhyve_api::VM_MMAP_MEMSEG, &mut map) }
    }

    /// Looks up a segment by `segid` and returns the offset
    /// within the guest's address virtual address space where
    /// it is mapped.
    pub fn devmem_offset(&self, segid: i32) -> Result<usize> {
        let mut devoff = bhyve_api::vm_devmem_offset { segid, offset: 0 };
        unsafe {
            self.ioctl(bhyve_api::VM_DEVMEM_GETOFFSET, &mut devoff)?;
        }

        assert!(devoff.offset >= 0);
        Ok(devoff.offset as usize)
    }

    /// Tracks dirty pages in the guest's physical address space.
    ///
    /// # Resetting Dirty Bits
    ///
    /// On versions of Bhyve prior to [`bhyve_api::ApiVersion::V17`], the
    ///
    /// # Arguments:
    /// - `start_gpa`: The start of the guest physical address range to track.
    /// Must be page aligned.
    /// - `bitmap`: A mutable bitmap of dirty pages, one bit per guest PFN
    /// relative to `start_gpa`.
    /// - `reset`: If `true`, the dirty bits in each guest page table entry in
    ///   the tracked range will be cleared as part of this operation. If
    ///   `false`, and the bhyve API version is at least
    ///   [`bhyve_api::ApiVersion::V17`], the tracked pages will remain marked
    ///   as dirty. If the bhyve API version is less than [`ApiVersion::V17`],
    ///   only `true` is supported.
    pub fn track_dirty_pages(
        &self,
        start_gpa: u64,
        bitmap: &mut [u8],
        reset: bool,
    ) -> Result<()> {
        if self.api_version()? >= bhyve_api::ApiVersion::V17 {
            // If the bhyve version is v17 or greater, always use the
            // `VM_NPT_OPERATION` ioctl rather than `VM_TRACK_DIRTY_PAGES`.
            let mut vno_operation =
                bhyve_api::VNO_OP_GET_DIRTY | bhyve_api::VNO_FLAG_BITMAP_OUT;
            if reset {
                vno_operation |= bhyve_api::VNO_OP_RESET_DIRTY;
            }
            let mut npt_op = bhyve_api::vm_npt_operation {
                vno_gpa: start_gpa,
                vno_len: (bitmap.len() * 8 * PAGE_SIZE) as u64,
                vno_bitmap: bitmap.as_mut_ptr(),
                vno_operation,
            };
            return unsafe {
                self.ioctl(bhyve_api::VM_NPT_OPERATION, &mut npt_op)
            };
        } else if !reset {
            // We were asked not to reset dirty bits, but this is only possible
            // on bhyve versions that support `VM_NPT_OPERATION``.
            return Err(Error::new(
                ErrorKind::Unsupported,
                "VmmHdl::track_dirty_pages(..., reset: false) is only supported on bhyve v17 or later",
            ));
        }

        let mut tracker = bhyve_api::vmm_dirty_tracker {
            vdt_start_gpa: start_gpa,
            vdt_len: bitmap.len() * 8 * PAGE_SIZE,
            vdt_pfns: bitmap.as_mut_ptr() as *mut c_void,
        };
        unsafe { self.ioctl(bhyve_api::VM_TRACK_DIRTY_PAGES, &mut tracker) }
    }

    /// Returns whether or not the [`VmmHdl::track_dirty_pages`] must reset the
    /// dirty bit in the guest's page table entries.
    ///
    /// If this method returns `true`, then calls to
    /// [`VmmHdl::track_dirty_pages`] must set the `reset` argument to `true`.
    pub fn must_reset_dirty_pages(&self) -> bool {
        self.api_version()
            .map(|v| v >= bhyve_api::ApiVersion::V17)
            // If we couldn't read the Bhyve API version, assume the operation
            // is unsupported.
            .unwrap_or(false)
    }

    /// Issues a request to update the virtual RTC time.
    pub fn rtc_settime(&self, time: Duration) -> Result<()> {
        self.inner.rtc_settime(time)
    }
    /// Writes to the registers within the RTC device.
    pub fn rtc_write(&self, offset: u8, value: u8) -> Result<()> {
        let mut data = bhyve_api::vm_rtc_data { offset: offset as i32, value };
        unsafe { self.ioctl(bhyve_api::VM_RTC_WRITE, &mut data) }
    }
    /// Reads from the registers within the RTC device.
    pub fn rtc_read(&self, offset: u8) -> Result<u8> {
        let mut data =
            bhyve_api::vm_rtc_data { offset: offset as i32, value: 0 };
        unsafe {
            self.ioctl(bhyve_api::VM_RTC_READ, &mut data)?;
        }
        Ok(data.value)
    }

    /// Asserts the requested IRQ for the virtual interrupt controller.
    ///
    /// `pic_irq` sends a request to the legacy 8259 PIC.
    /// `ioapic_irq` (if supplied) sends a request to the IOAPIC.
    pub fn isa_assert_irq(
        &self,
        pic_irq: u8,
        ioapic_irq: Option<u8>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq {
            atpic_irq: pic_irq as i32,
            ioapic_irq: ioapic_irq.map(|x| x as i32).unwrap_or(-1),
        };
        unsafe { self.ioctl(bhyve_api::VM_ISA_ASSERT_IRQ, &mut data) }
    }
    /// Deasserts the requested IRQ.
    pub fn isa_deassert_irq(
        &self,
        pic_irq: u8,
        ioapic_irq: Option<u8>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq {
            atpic_irq: pic_irq as i32,
            ioapic_irq: ioapic_irq.map(|x| x as i32).unwrap_or(-1),
        };
        unsafe { self.ioctl(bhyve_api::VM_ISA_DEASSERT_IRQ, &mut data) }
    }
    /// Pulses the requested IRQ, turning it on then off.
    pub fn isa_pulse_irq(
        &self,
        pic_irq: u8,
        ioapic_irq: Option<u8>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq {
            atpic_irq: pic_irq as i32,
            ioapic_irq: ioapic_irq.map(|x| x as i32).unwrap_or(-1),
        };
        unsafe { self.ioctl(bhyve_api::VM_ISA_PULSE_IRQ, &mut data) }
    }
    #[allow(unused)]
    pub fn isa_set_trigger_mode(
        &self,
        vec: u8,
        level_mode: bool,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_isa_irq_trigger {
            atpic_irq: vec as i32,
            trigger: if level_mode { 1 } else { 0 },
        };
        unsafe { self.ioctl(bhyve_api::VM_ISA_SET_IRQ_TRIGGER, &mut data) }
    }

    #[allow(unused)]
    pub fn ioapic_assert_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        unsafe { self.ioctl(bhyve_api::VM_IOAPIC_ASSERT_IRQ, &mut data) }
    }
    #[allow(unused)]
    pub fn ioapic_deassert_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        unsafe { self.ioctl(bhyve_api::VM_IOAPIC_DEASSERT_IRQ, &mut data) }
    }
    #[allow(unused)]
    pub fn ioapic_pulse_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        unsafe { self.ioctl(bhyve_api::VM_IOAPIC_PULSE_IRQ, &mut data) }
    }
    #[allow(unused)]
    pub fn ioapic_pin_count(&self) -> Result<u8> {
        let mut data = 0u32;
        unsafe {
            self.ioctl(bhyve_api::VM_IOAPIC_PINCOUNT, &mut data)?;
        }
        Ok(data as u8)
    }

    pub fn lapic_msi(&self, addr: u64, msg: u64) -> Result<()> {
        let mut data = bhyve_api::vm_lapic_msi { msg, addr };
        unsafe { self.ioctl(bhyve_api::VM_LAPIC_MSI, &mut data) }
    }

    pub fn pmtmr_locate(&self, port: u16) -> Result<()> {
        unsafe { self.ioctl(bhyve_api::VM_PMTMR_LOCATE, port as *mut usize) }
    }

    pub fn suspend(
        &self,
        how: bhyve_api::vm_suspend_how,
        source: Option<i32>,
    ) -> Result<()> {
        let mut data = bhyve_api::vm_suspend {
            how: how as u32,
            source: source.unwrap_or(-1),
        };
        unsafe { self.ioctl(bhyve_api::VM_SUSPEND, &mut data) }
    }

    pub fn reinit(&self, force_suspend: bool) -> Result<()> {
        let mut data = bhyve_api::vm_reinit { flags: 0 };
        if force_suspend {
            data.flags |= bhyve_api::VM_REINIT_F_FORCE_SUSPEND;
        }
        unsafe { self.ioctl(bhyve_api::VM_REINIT, &mut data) }
    }

    /// Pause device emulation logic for the instance (such as timers, etc).
    /// This allows a consistent snapshot to be taken or loaded.
    pub fn pause(&self) -> Result<()> {
        self.ioctl_usize(bhyve_api::VM_PAUSE, 0)
    }

    /// Resume device emulation logic from a prior [VmmHdl::pause] call.
    pub fn resume(&self) -> Result<()> {
        self.ioctl_usize(bhyve_api::VM_RESUME, 0)
    }

    /// Destroys the VMM.
    // TODO: Should this take "mut self", to consume the object?
    pub fn destroy(&self) -> Result<()> {
        if self.destroyed.swap(true, Ordering::SeqCst) {
            return Err(Error::new(ErrorKind::NotFound, "already destroyed"));
        }

        // Attempt destruction via the handle (rather than going through vmmctl)
        // This is done through the [ioctl_usize] helper rather than
        // [Self::ioctl_usize], since the latter rejects attempted operations
        // after `destroyed` is set.
        if let Ok(_) = self.inner.ioctl_usize(bhyve_api::VM_DESTROY_SELF, 0) {
            return Ok(());
        }

        // If that failed (which may occur on older platforms without
        // self-destruction), then fall back to performing the destroy through
        // the vmmctl device.
        let ctl = bhyve_api::VmmCtlFd::open()?;
        ctl.vm_destroy(self.name.as_bytes()).or_else(|e| match e.kind() {
            ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
    }

    /// Set whether instance should auto-destruct when closed
    pub fn set_autodestruct(&self, enable_autodestruct: bool) -> Result<()> {
        self.ioctl_usize(
            bhyve_api::VM_SET_AUTODESTRUCT,
            enable_autodestruct as usize,
        )
    }

    pub fn data_op(&self, class: u16, version: u16) -> bhyve_api::VmmDataOp {
        self.inner.data_op(class, version)
    }
}

#[cfg(test)]
impl VmmHdl {
    /// Build a VmmHdl instance suitable for unit tests, but nothing else, since
    /// it will not be backed by any real vmm resources.
    pub(crate) fn new_test(mem_size: usize) -> Result<Self> {
        use tempfile::tempfile;
        let fp = tempfile()?;
        fp.set_len(mem_size as u64).unwrap();
        let inner = unsafe { bhyve_api::VmmFd::new_raw(fp) };
        Ok(Self {
            inner,
            destroyed: AtomicBool::new(false),
            name: "TEST-ONLY VMM INSTANCE".to_string(),
            is_test_hdl: true,
        })
    }
}

pub fn query_reservoir() -> Result<bhyve_api::vmm_resv_query> {
    let ctl = bhyve_api::VmmCtlFd::open()?;
    let mut data = bhyve_api::vmm_resv_query::default();
    let _ = unsafe { ctl.ioctl(bhyve_api::VMM_RESV_QUERY, &mut data) }?;
    Ok(data)
}
