//! Module responsible for communicating with the kernel's VMM.
//!
//! Responsible for both issuing commands to the bhyve
//! kernel controller to create and destroy VMs.
//!
//! Additionally, contains a wrapper struct ([`VmmHdl`])
//! for encapsulating commands to the underlying kernel
//! object which represents a single VM.

use erased_serde::{Deserializer, Serialize};

use super::mapping::*;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result, Write};
use std::os::raw::c_void;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::common::PAGE_SIZE;
use crate::migrate::MigrateStateError;
use crate::util::sys::ioctl;

#[derive(Default, Copy, Clone)]
/// Configurable options for VMM instance creation
///
/// # Options:
/// - `force`: If a VM with the name `name` already exists, attempt
///   to destroy the VM before creating it.
/// - `use_reservoir`: Allocate guest memory (only) from the VMM reservoir.  If
/// this is enabled, and memory in excess of what is available from the
/// reservoir is requested, creation of that guest memory resource will fail.
pub struct CreateOpts {
    pub force: bool,
    pub use_reservoir: bool,
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
pub(crate) fn create_vm(
    name: impl AsRef<str>,
    opts: CreateOpts,
) -> Result<VmmHdl> {
    create_vm_impl(name.as_ref(), opts)
}

#[cfg(target_os = "illumos")]
fn create_vm_impl(name: &str, opts: CreateOpts) -> Result<VmmHdl> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let ctlfd = ctl.as_raw_fd();

    let mut req = bhyve_api::vm_create_req::new(name);
    if opts.use_reservoir {
        req.flags |= bhyve_api::VCF_RESERVOIR_MEM;
    }
    let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, &req) };
    if res != 0 {
        let err = Error::last_os_error();
        if err.kind() != ErrorKind::AlreadyExists || !opts.force {
            return Err(err);
        }

        // try to nuke(!) the existing vm
        let dreq = bhyve_api::vm_destroy_req::new(name);
        let res =
            unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_DESTROY_VM, &dreq) };
        if res != 0 {
            let err = Error::last_os_error();
            if err.kind() != ErrorKind::NotFound {
                return Err(err);
            }
        }

        // now attempt to create in its presumed absence
        let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_CREATE_VM, &req) };
        if res != 0 {
            return Err(Error::last_os_error());
        }
    }

    let mut vmpath = PathBuf::from(bhyve_api::VMM_PATH_PREFIX);
    vmpath.push(name);

    let fp = OpenOptions::new().write(true).read(true).open(vmpath)?;

    // Safety: Files opened within VMM_PATH_PREFIX are VMMs, which may not be
    // truncated.
    let inner = unsafe { VmmFile::new(fp) };
    Ok(VmmHdl {
        inner,
        destroyed: AtomicBool::new(false),
        name: name.to_string(),
    })
}
#[cfg(not(target_os = "illumos"))]
fn create_vm_impl(_name: &str, _opts: CreateOpts) -> Result<VmmHdl> {
    {
        // suppress unused warnings
        let mut _oo = OpenOptions::new();
        _oo.mode(0o444);
        let _flag = libc::O_EXCL;
        let _pathbuf = PathBuf::new();
    }
    Err(Error::new(ErrorKind::Other, "illumos required"))
}

/// Destroys the virtual machine matching the provided `name`.
pub fn destroy_vm(name: impl AsRef<str>) -> Result<()> {
    destroy_vm_impl(name.as_ref())
}

#[cfg(target_os = "illumos")]
fn destroy_vm_impl(name: &str) -> Result<()> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let ctlfd = ctl.as_raw_fd();

    let dreq = bhyve_api::vm_destroy_req::new(name);
    let res = unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_DESTROY_VM, &dreq) };
    if res != 0 {
        let err = Error::last_os_error();
        if err.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}
#[cfg(not(target_os = "illumos"))]
fn destroy_vm_impl(_name: &str) -> Result<()> {
    Ok(())
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
    pub(super) inner: VmmFile,
    destroyed: AtomicBool,
    name: String,
}
impl VmmHdl {
    /// Accesses the raw file descriptor behind the VMM.
    pub fn fd(&self) -> RawFd {
        self.inner.0.as_raw_fd()
    }
    /// Sends an ioctl to the underlying VMM.
    pub fn ioctl<T>(&self, cmd: i32, data: *mut T) -> Result<()> {
        if self.destroyed.load(Ordering::Acquire) {
            return Err(Error::new(ErrorKind::NotFound, "instance destroyed"));
        }
        ioctl(self.fd(), cmd, data)?;
        Ok(())
    }
    /// Creates and sends a request to allocate a memory segment within the VM.
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
        self.ioctl(bhyve_api::VM_ALLOC_MEMSEG, &mut seg)
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
        self.ioctl(bhyve_api::VM_MMAP_MEMSEG, &mut map)
    }

    /// Looks up a segment by `segid` and returns the offset
    /// within the guest's address virtual address space where
    /// it is mapped.
    pub fn devmem_offset(&self, segid: i32) -> Result<usize> {
        let mut devoff = bhyve_api::vm_devmem_offset { segid, offset: 0 };
        self.ioctl(bhyve_api::VM_DEVMEM_GETOFFSET, &mut devoff)?;

        assert!(devoff.offset >= 0);
        Ok(devoff.offset as usize)
    }

    /// Maps a memory segment into propolis' address space.
    ///
    /// Returns a pointer to the mapped segment, if successful.
    pub fn mmap_seg(&self, segid: i32, size: usize) -> Result<Mapping> {
        let devoff = self.devmem_offset(segid)?;

        Mapping::new(size, Prot::READ | Prot::WRITE, &self.inner, devoff as i64)
    }

    /// Maps a portion of the guest's virtual address space into propolis'
    /// address space.
    ///
    /// # Arguments:
    /// - `offset`: Offset within the guests's address space to be mapped.
    /// - `size`: Size of the mapping.
    /// - `prot`: Memory protections to be applied to the mapping.
    ///
    /// Return the mapped segment if successful.
    pub fn mmap_guest_mem(
        &self,
        guard_space: &mut GuardSpace,
        offset: usize,
        size: usize,
        prot: Prot,
    ) -> Result<Mapping> {
        guard_space.mapping(size, prot, &self.inner, offset as i64)
    }

    /// Tracks dirty pages in the guest's physical address space.
    ///
    /// # Arguments:
    /// - `start_gpa`: The start of the guest physical address range to track.
    /// Must be page aligned.
    /// - `bitmap`: A mutable bitmap of dirty pages, one bit per guest PFN
    /// relative to `start_gpa`.
    pub fn track_dirty_pages(
        &self,
        start_gpa: u64,
        bitmap: &mut [u8],
    ) -> Result<()> {
        let mut tracker = bhyve_api::vmm_dirty_tracker {
            vdt_start_gpa: start_gpa,
            vdt_len: bitmap.len() * 8 * PAGE_SIZE,
            vdt_pfns: bitmap.as_mut_ptr() as *mut c_void,
        };
        self.ioctl(bhyve_api::VM_TRACK_DIRTY_PAGES, &mut tracker)
    }

    /// Issues a request to update the virtual RTC time.
    pub fn rtc_settime(&self, unix_time: u64) -> Result<()> {
        let mut time: u64 = unix_time;
        self.ioctl(bhyve_api::VM_RTC_SETTIME, &mut time)
    }
    /// Writes to the registers within the RTC device.
    pub fn rtc_write(&self, offset: u8, value: u8) -> Result<()> {
        let mut data = bhyve_api::vm_rtc_data { offset: offset as i32, value };
        self.ioctl(bhyve_api::VM_RTC_WRITE, &mut data)
    }
    /// Reads from the registers within the RTC device.
    pub fn rtc_read(&self, offset: u8) -> Result<u8> {
        let mut data =
            bhyve_api::vm_rtc_data { offset: offset as i32, value: 0 };
        self.ioctl(bhyve_api::VM_RTC_READ, &mut data)?;
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
        self.ioctl(bhyve_api::VM_ISA_ASSERT_IRQ, &mut data)
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
        self.ioctl(bhyve_api::VM_ISA_DEASSERT_IRQ, &mut data)
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
        self.ioctl(bhyve_api::VM_ISA_PULSE_IRQ, &mut data)
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
        self.ioctl(bhyve_api::VM_ISA_SET_IRQ_TRIGGER, &mut data)
    }

    #[allow(unused)]
    pub fn ioapic_assert_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        self.ioctl(bhyve_api::VM_IOAPIC_ASSERT_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn ioapic_deassert_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        self.ioctl(bhyve_api::VM_IOAPIC_DEASSERT_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn ioapic_pulse_irq(&self, irq: u8) -> Result<()> {
        let mut data = bhyve_api::vm_ioapic_irq { irq: irq as i32 };
        self.ioctl(bhyve_api::VM_IOAPIC_PULSE_IRQ, &mut data)
    }
    #[allow(unused)]
    pub fn ioapic_pin_count(&self) -> Result<u8> {
        let mut data = 0u32;
        self.ioctl(bhyve_api::VM_IOAPIC_PINCOUNT, &mut data)?;
        Ok(data as u8)
    }

    pub fn lapic_msi(&self, addr: u64, msg: u64) -> Result<()> {
        let mut data = bhyve_api::vm_lapic_msi { msg, addr };
        self.ioctl(bhyve_api::VM_LAPIC_MSI, &mut data)
    }

    pub fn pmtmr_locate(&self, port: u16) -> Result<()> {
        self.ioctl(bhyve_api::VM_PMTMR_LOCATE, port as *mut usize)
    }

    pub fn suspend(&self, how: bhyve_api::vm_suspend_how) -> Result<()> {
        let mut data = bhyve_api::vm_suspend { how: how as u32 };
        self.ioctl(bhyve_api::VM_SUSPEND, &mut data)
    }

    pub fn reinit(&self, force_suspend: bool) -> Result<()> {
        let mut data = bhyve_api::vm_reinit { flags: 0 };
        if force_suspend {
            data.flags |= bhyve_api::VM_REINIT_F_FORCE_SUSPEND;
        }
        self.ioctl(bhyve_api::VM_REINIT, &mut data)
    }

    /// Destroys the VMM.
    // TODO: Should this take "mut self", to consume the object?
    pub fn destroy(&self) -> Result<()> {
        if self.destroyed.swap(true, Ordering::SeqCst) {
            Err(Error::new(ErrorKind::NotFound, "already destroyed"))
        } else {
            destroy_vm(&self.name)
        }
    }

    /// Export the global VMM state.
    pub fn export(
        &self,
    ) -> std::result::Result<Box<dyn Serialize>, MigrateStateError> {
        Ok(Box::new(migrate::BhyveVmV1::read(self)?))
    }

    /// Restore previously exported global VMM state.
    pub fn import(
        &self,
        deserializer: &mut dyn Deserializer,
    ) -> std::result::Result<(), MigrateStateError> {
        let deserialized: migrate::BhyveVmV1 =
            erased_serde::deserialize(deserializer)?;
        deserialized.write(self)?;
        Ok(())
    }
}

#[cfg(test)]
impl VmmHdl {
    /// Build a VmmHdl instance suitable for unit tests, but nothing else, since
    /// it will not be backed by any real vmm reousrces.
    pub(crate) fn new_test() -> Result<Self> {
        use tempfile::tempfile;
        // Create a 2M temp file to use as our VM "memory"
        let fp = tempfile()?;
        fp.set_len(2 * 1024 * 1024).unwrap();
        Ok(Self {
            inner: VmmFile(fp),
            destroyed: AtomicBool::new(false),
            name: "TEST-ONLY VMM INSTANCE".to_string(),
        })
    }
}

#[cfg(target_os = "illumos")]
pub fn query_reservoir() -> Result<bhyve_api::vmm_resv_query> {
    let ctl = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(bhyve_api::VMM_CTL_PATH)?;
    let ctlfd = ctl.as_raw_fd();

    let mut data = bhyve_api::vmm_resv_query::default();
    let res =
        unsafe { libc::ioctl(ctlfd, bhyve_api::VMM_RESV_QUERY, &mut data) };
    if res != 0 {
        Err(Error::last_os_error())
    } else {
        Ok(data)
    }
}
#[cfg(not(target_os = "illumos"))]
pub fn query_reservoir() -> Result<bhyve_api::vmm_resv_query> {
    Err(Error::new(ErrorKind::Other, "illumos required"))
}

pub mod migrate {
    use std::io;

    use bhyve_api::vdi_field_entry_v1;
    use serde::{Deserialize, Serialize};

    use crate::vmm;

    use super::VmmHdl;

    #[derive(Clone, Debug, Default, Deserialize, Serialize)]
    pub struct BhyveVmV1 {
        arch_entries: Vec<ArchEntryV1>,
    }

    #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize)]
    pub struct ArchEntryV1 {
        pub ident: u32,
        pub value: u64,
    }

    impl From<vdi_field_entry_v1> for ArchEntryV1 {
        fn from(raw: vdi_field_entry_v1) -> Self {
            Self { ident: raw.vfe_ident, value: raw.vfe_value }
        }
    }
    impl From<ArchEntryV1> for vdi_field_entry_v1 {
        fn from(entry: ArchEntryV1) -> Self {
            vdi_field_entry_v1 {
                vfe_ident: entry.ident,
                vfe_value: entry.value,
                ..Default::default()
            }
        }
    }

    impl BhyveVmV1 {
        pub(super) fn read(hdl: &VmmHdl) -> io::Result<Self> {
            let arch_entries: Vec<bhyve_api::vdi_field_entry_v1> =
                vmm::data::read_many(hdl, -1, bhyve_api::VDC_VMM_ARCH, 1)?;
            Ok(Self {
                arch_entries: arch_entries
                    .into_iter()
                    .map(From::from)
                    .collect(),
            })
        }

        pub(super) fn write(self, hdl: &VmmHdl) -> io::Result<()> {
            let mut arch_entries: Vec<bhyve_api::vdi_field_entry_v1> = self
                .arch_entries
                .into_iter()
                // TODO: Guest TSC frequency is not currently adjustable
                .filter(|e| e.ident != bhyve_api::VAI_TSC_FREQ)
                .map(From::from)
                .collect();
            vmm::data::write_many(
                hdl,
                -1,
                bhyve_api::VDC_VMM_ARCH,
                1,
                &mut arch_entries,
            )?;
            Ok(())
        }
    }
}
