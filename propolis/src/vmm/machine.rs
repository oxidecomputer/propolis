//! Representation of a virtual machine's hardware.

use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::{Arc, Mutex};

use crate::common::{GuestAddr, GuestRegion};
use crate::hw::rtc::Rtc;
use crate::mmio::MmioBus;
use crate::pio::PioBus;
use crate::util::aspace::ASpace;
use crate::vcpu::VcpuHdl;
use crate::vmm::{create_vm, GuardSpace, Mapping, Prot, SubMapping, VmmHdl};

// XXX: Arbitrary limits for now
pub const MAX_PHYSMEM: usize = 0x80_0000_0000;
pub const MAX_SYSMEM: usize = 0x40_0000_0000;

#[derive(Copy, Clone)]
enum MapKind {
    SysMem(i32, Prot),
    Rom(i32, Prot),
    MmioReserve,
}

struct MapEnt {
    kind: MapKind,
    name: String,
    // A mapping of the guest address space within the current process.
    guest_map: Option<Mapping>,
    dev_map: Option<Mutex<Mapping>>,
}

/// The aggregate representation of a virtual machine.
///
/// This includes:
/// - The underlying [`VmmHdl`], accessible via [`Machine::get_hdl`].
/// - The device's physical memory representation
/// - Buses.
pub struct Machine {
    hdl: Arc<VmmHdl>,
    max_cpu: u8,

    _guard_space: GuardSpace,
    map_physmem: ASpace<MapEnt>,
    bus_mmio: MmioBus,
    bus_pio: PioBus,
}

impl Machine {
    /// Walks through the machine's address space until a
    /// ROM entry named `name` is found, then invoke `func`
    /// on the entry's device mapping.
    pub fn populate_rom<F>(&self, name: &str, func: F) -> Result<()>
    where
        F: FnOnce(&Mapping) -> Result<()>,
    {
        let (_addr, _len, ent) = self
            .map_physmem
            .iter()
            .find(|(_addr, _len, ent)| match ent.kind {
                MapKind::Rom(_, _) => ent.name == name,
                _ => false,
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("rom {} not found", name),
                )
            })?;
        assert!(ent.dev_map.is_some());
        let mapping = ent.dev_map.as_ref().unwrap().lock().unwrap();
        func(&*mapping)
    }

    /// Initialize the real-time-clock of the device.
    pub fn initialize_rtc(&self, lowmem: usize, highmem: usize) -> Result<()> {
        Rtc::set_time(&self.hdl)?;
        Rtc::store_memory_sizing(&self.hdl, lowmem, highmem)?;
        Ok(())
    }

    pub fn reinitialize(&self) -> Result<()> {
        self.hdl.reinit(true)?;
        // Since VM_REINIT unmaps all non-sysmem segments from the address space
        // of the VM, we must reestablish the ROM mapping(s) now.
        for (addr, len, ent) in self.map_physmem.iter() {
            if let MapKind::Rom(segid, prot) = ent.kind {
                self.hdl.map_memseg(segid, addr, len, 0, prot)?;
            }
        }
        Ok(())
    }

    /// Get a handle to the underlying VMM.
    pub fn get_hdl(&self) -> Arc<VmmHdl> {
        Arc::clone(&self.hdl)
    }

    /// Get a handle to the underlying VCPU.
    pub fn vcpu(&self, id: usize) -> VcpuHdl {
        assert!(id <= self.max_cpu as usize);
        VcpuHdl::new(self.get_hdl(), id as i32)
    }
}

/// Wrapper around a [`Machine`] object which exposes helpers for
/// accessing different aspects of the VMM.
#[derive(Clone)]
pub struct MachineCtx {
    vm: Arc<Machine>,
}

impl MachineCtx {
    pub fn new(vm: &Arc<Machine>) -> Self {
        Self { vm: Arc::clone(vm) }
    }

    pub fn pio(&self) -> &PioBus {
        &self.vm.bus_pio
    }

    pub fn mmio(&self) -> &MmioBus {
        &self.vm.bus_mmio
    }

    pub fn hdl(&self) -> &VmmHdl {
        &self.vm.hdl
    }

    pub fn vcpu(&self, id: usize) -> VcpuHdl {
        self.vm.vcpu(id)
    }

    pub fn memctx(&self) -> MemCtx<'_> {
        MemCtx::new(self)
    }
    pub fn max_cpus(&self) -> usize {
        self.vm.max_cpu as usize
    }
    pub fn vcpus(&self) -> Vcpus<'_> {
        Vcpus { mctx: self, id: 0 }
    }
}

pub struct Vcpus<'a> {
    mctx: &'a MachineCtx,
    id: usize,
}
impl Iterator for Vcpus<'_> {
    type Item = VcpuHdl;

    fn next(&mut self) -> Option<Self::Item> {
        if self.id < self.mctx.max_cpus() {
            let vcpu = self.mctx.vcpu(self.id);
            self.id += 1;
            Some(vcpu)
        } else {
            None
        }
    }
}

/// Wrapper around an address space for a VM.
pub struct MemCtx<'a> {
    map: &'a ASpace<MapEnt>,
}
impl<'a> MemCtx<'a> {
    fn new(mctx: &'a MachineCtx) -> Self {
        Self { map: &mctx.vm.map_physmem }
    }
    /// Reads a generic value from a specified guest address.
    pub fn read<T: Copy>(&self, addr: GuestAddr) -> Option<T> {
        if let Some(mapping) =
            self.region_covered(addr, size_of::<T>(), Prot::READ)
        {
            mapping.read().ok()
        } else {
            None
        }
    }
    /// Reads bytes into a requested buffer from guest memory.
    ///
    /// Copies up to `buf.len()` or `len` bytes, whichever is smaller.
    pub fn read_into(
        &self,
        addr: GuestAddr,
        buf: &mut [u8],
        len: usize,
    ) -> Option<usize> {
        let len = usize::min(buf.len(), len);
        if let Some(mapping) = self.region_covered(addr, len, Prot::READ) {
            mapping.read_bytes(&mut buf[..len]).ok()
        } else {
            None
        }
    }
    /// Reads multiple objects from a guest address.
    pub fn read_many<T: Copy>(
        &self,
        base: GuestAddr,
        count: usize,
    ) -> Option<MemMany<T>> {
        self.region_covered(base, size_of::<T>() * count, Prot::READ).map(
            |mapping| MemMany { mapping, pos: 0, count, phantom: PhantomData },
        )
    }
    /// Writes a value to guest memory.
    pub fn write<T: Copy>(&self, addr: GuestAddr, val: &T) -> bool {
        if let Some(mapping) =
            self.region_covered(addr, size_of::<T>(), Prot::WRITE)
        {
            mapping.write(val).is_ok()
        } else {
            false
        }
    }
    /// Writes bytes from a buffer to guest memory.
    ///
    /// Writes up to `buf.len()` or `len` bytes, whichever is smaller.
    pub fn write_from(
        &self,
        addr: GuestAddr,
        buf: &[u8],
        len: usize,
    ) -> Option<usize> {
        let len = usize::min(buf.len(), len);
        if let Some(mapping) = self.region_covered(addr, len, Prot::WRITE) {
            mapping.write_bytes(&buf[..len]).ok()
        } else {
            None
        }
    }
    /// Writes a single value to guest memory.
    pub fn write_byte(&self, addr: GuestAddr, val: u8, count: usize) -> bool {
        if let Some(mapping) = self.region_covered(addr, count, Prot::WRITE) {
            mapping.write_byte(val, count).is_ok()
        } else {
            false
        }
    }

    pub fn writable_region(&self, region: &GuestRegion) -> Option<SubMapping> {
        let mapping = self.region_covered(region.0, region.1, Prot::WRITE)?;
        Some(mapping)
    }
    pub fn readable_region(&self, region: &GuestRegion) -> Option<SubMapping> {
        let mapping = self.region_covered(region.0, region.1, Prot::READ)?;
        Some(mapping)
    }

    // Looks up a region of memory in the guest's address space,
    // returning a pointer to the containing region.
    fn region_covered(
        &self,
        addr: GuestAddr,
        len: usize,
        need_prot: Prot,
    ) -> Option<SubMapping> {
        let start = addr.0 as usize;
        let end = start + len;
        if let Ok((addr, rlen, ent)) = self.map.region_at(start) {
            if addr + rlen < end {
                return None;
            }
            let req_offset = start - addr;
            match ent.kind {
                MapKind::SysMem(_, prot) | MapKind::Rom(_, prot) => {
                    // TODO: This protection check may be redundant with
                    // mapping, which has its own memory protection tracking.
                    if prot.contains(need_prot) {
                        let base = ent.guest_map.as_ref().unwrap().as_ref();
                        return base.subregion(req_offset, len);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// A contiguous region of memory containing generic objects.
pub struct MemMany<'a, T: Copy> {
    mapping: SubMapping<'a>,
    count: usize,
    pos: usize,
    phantom: PhantomData<T>,
}
impl<'a, T: Copy> MemMany<'a, T> {
    /// Gets the object at position `pos` within the memory region.
    ///
    /// Returns [`Option::None`] if out of range.
    pub fn get(&self, pos: usize) -> Option<T> {
        if pos < self.count {
            let sz = std::mem::size_of::<T>();
            self.mapping.subregion(pos * sz, sz)?.read().ok()
        } else {
            None
        }
    }
}
impl<'a, T: Copy> Iterator for MemMany<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let res = self.get(self.pos);
        self.pos += 1;
        res
    }
}

/// A builder object which may be used to initialize an instance.
///
/// # Example
///
/// ```no_run
/// use propolis::vmm::{Builder, Prot};
/// use propolis::instance::Instance;
///
/// let builder = Builder::new("my-machine", true).unwrap()
///     .max_cpus(4).unwrap()
///     .add_mem_region(0, 0xc000_0000, Prot::ALL, "lowmem").unwrap()
///     .add_mem_region(0x1_0000_0000, 0xc000_0000, Prot::ALL, "highmem").unwrap()
///     .add_rom_region(0xffe0_0000, 0x20_0000, Prot::READ | Prot::EXEC, "bootrom")
///         .unwrap()
///     .add_mmio_region(0xc0000000_usize, 0x20000000_usize, "dev32").unwrap();
/// let inst = Instance::create(builder, propolis::vcpu_run_loop).unwrap();
/// ```
pub struct Builder {
    inner_hdl: Option<VmmHdl>,
    max_cpu: u8,
    cur_segid: i32,
    memmap: ASpace<(MapKind, String)>,
}
impl Builder {
    /// Constructs a new builder object which may be used
    /// to produce a VM.
    ///
    /// In the construction of this object, the builder
    /// attempts to access the vmm controller at "/dev/vmmctl",
    /// and issues commands to begin construction of the VM.
    ///
    /// # Arguments
    /// - `name`: The name for the new instance.
    /// - `force`: If true, deletes the VM if it already exists.
    pub fn new(name: &str, force: bool) -> Result<Self> {
        let hdl = create_vm(name, force)?;
        Ok(Self {
            inner_hdl: Some(hdl),
            max_cpu: 1,
            cur_segid: 0,
            memmap: ASpace::new(0, MAX_PHYSMEM - 1),
        })
    }
    fn hdl(&self) -> &VmmHdl {
        self.inner_hdl.as_ref().unwrap()
    }
    fn next_segid(&mut self) -> i32 {
        let next = self.cur_segid;
        self.cur_segid += 1;
        next
    }

    /// Creates and maps a memory segment in the guest's address space,
    /// identified as system memory.
    pub fn add_mem_region(
        mut self,
        start: usize,
        len: usize,
        prot: Prot,
        name: &str,
    ) -> Result<Self> {
        let segid = self.next_segid();
        self.hdl().create_memseg(segid, len, None)?;
        self.hdl().map_memseg(segid, start, len, 0, prot)?;
        self.memmap
            .register(
                start,
                len,
                (MapKind::SysMem(segid, prot), name.to_string()),
            )
            .map_err(|_| {
                Error::new(ErrorKind::AlreadyExists, "addr conflict")
            })?;
        Ok(self)
    }

    /// Creates and maps a memory segment in the guest's address space,
    /// identified as ROM.
    ///
    /// Since this is ROM, an error will be returned if `prot` includes
    /// [`Prot::WRITE`].
    pub fn add_rom_region(
        mut self,
        start: usize,
        len: usize,
        prot: Prot,
        name: &str,
    ) -> Result<Self> {
        if prot.contains(Prot::WRITE) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "ROM cannot be writable",
            ));
        }
        let segid = self.next_segid();
        self.hdl().create_memseg(segid, len, Some(name))?;
        self.hdl().map_memseg(segid, start, len, 0, prot)?;
        self.memmap
            .register(start, len, (MapKind::Rom(segid, prot), name.to_string()))
            .map_err(|_| {
                Error::new(ErrorKind::AlreadyExists, "addr conflict")
            })?;
        Ok(self)
    }
    /// Registers a region of memory for MMIO.
    pub fn add_mmio_region(
        mut self,
        start: usize,
        len: usize,
        name: &str,
    ) -> Result<Self> {
        self.memmap
            .register(start, len, (MapKind::MmioReserve, name.to_string()))
            .map_err(|_| {
                Error::new(ErrorKind::AlreadyExists, "addr conflict")
            })?;
        Ok(self)
    }
    /// Sets the maximum number of CPUs for the machine.
    pub fn max_cpus(mut self, max: u8) -> Result<Self> {
        if max == 0 || max > bhyve_api::VM_MAXCPU as u8 {
            Err(Error::new(ErrorKind::InvalidInput, "maxcpu out of range"))
        } else {
            self.max_cpu = max;
            Ok(self)
        }
    }

    fn highest_guest_addr(&self) -> usize {
        self.memmap.iter().fold(0, |highest, (start, len, _map)| {
            let end = start + len;
            if highest > end {
                highest
            } else {
                end
            }
        })
    }

    fn prep_mem_map(
        &self,
        hdl: &VmmHdl,
    ) -> Result<(GuardSpace, ASpace<MapEnt>)> {
        let mut guard_space = GuardSpace::new(self.highest_guest_addr())?;

        let mut map = ASpace::new(0, MAX_PHYSMEM);
        for (start, len, (ent, name)) in self.memmap.iter() {
            let (guest_map, dev_map) = match *ent {
                MapKind::SysMem(_, prot) => {
                    let mapping = hdl.mmap_guest_mem(
                        &mut guard_space,
                        start,
                        len,
                        prot & (Prot::READ | Prot::WRITE),
                    )?;
                    (Some(mapping), None)
                }
                MapKind::Rom(segid, _) => {
                    // Only PROT_READ makes sense for normal ROM access
                    let mapping = hdl.mmap_guest_mem(
                        &mut guard_space,
                        start,
                        len,
                        Prot::READ,
                    )?;
                    let dev = hdl.mmap_seg(segid, len)?;
                    (Some(mapping), Some(Mutex::new(dev)))
                }
                MapKind::MmioReserve => (None, None),
            };
            map.register(
                start,
                len,
                MapEnt { kind: *ent, name: name.clone(), guest_map, dev_map },
            )
            .unwrap();
        }

        Ok((guard_space, map))
    }

    /// Consumes `self` and creates a new [`Machine`] based
    /// on the provided memory regions.
    pub fn finalize(mut self) -> Result<Machine> {
        let hdl = std::mem::replace(&mut self.inner_hdl, None).unwrap();

        let (guard_space, map) = self.prep_mem_map(&hdl)?;

        let arc_hdl = Arc::new(hdl);

        let machine = Machine {
            hdl: arc_hdl,
            max_cpu: self.max_cpu,

            _guard_space: guard_space,
            map_physmem: map,
            bus_mmio: MmioBus::new(MAX_PHYSMEM),
            bus_pio: PioBus::new(),
        };
        Ok(machine)
    }
}
impl Drop for Builder {
    fn drop(&mut self) {
        if self.inner_hdl.is_some() {
            // Do not allow the vmm device to persist
            self.inner_hdl.as_mut().unwrap().destroy().unwrap();
        }
    }
}
