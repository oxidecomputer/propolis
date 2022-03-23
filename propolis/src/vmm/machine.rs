//! Representation of a virtual machine's hardware.

use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use crate::common::{GuestAddr, GuestRegion};
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
    /// Mapping of the guest address space within the current process, subject
    /// to the same protection restrictions as the guest.
    guest_map: Option<Mapping>,
    /// Mapping of vm memory segment within current process, with full (read and
    /// write) access to its contents.
    seg_map: Option<Mapping>,
}

/// The aggregate representation of a virtual machine.
///
/// This includes:
/// - The underlying [`VmmHdl`], accessible via [`Machine::get_hdl`].
/// - The device's physical memory representation
/// - Buses.
pub struct Machine {
    pub hdl: Arc<VmmHdl>,
    max_cpu: u8,

    _guard_space: GuardSpace,
    map_physmem: ASpace<MapEnt>,
    pub bus_mmio: Arc<MmioBus>,
    pub bus_pio: Arc<PioBus>,
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
        assert!(ent.seg_map.is_some());
        let mapping = ent.seg_map.as_ref().unwrap();
        func(&*mapping)
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
impl Drop for Machine {
    fn drop(&mut self) {
        // Clear all of the entries from the physmem map so their associated
        // mappings in the process address space are munmapped.
        self.map_physmem.clear();

        // Only after that should the VM instance be destroyed.
        //
        // TODO: For debugging purposes, we may want to skip destruction under
        // certain circumstances to inspect the persisting in-kernel state.
        let _ = self.hdl.destroy();
    }
}

#[cfg(test)]
impl Machine {
    pub(crate) fn new_test() -> Result<Arc<Self>> {
        let hdl = VmmHdl::new_test()?;

        // TODO: meaningfully populate these
        let guard_space = GuardSpace::new(crate::common::PAGE_SIZE)?;
        let mut map = ASpace::new(0, MAX_PHYSMEM);
        map.register(
            0,
            1024 * 1024,
            MapEnt {
                kind: MapKind::SysMem(0, Prot::READ),
                name: "test-readable".to_string(),
                guest_map: Some(Mapping::new(
                    1024 * 1024,
                    Prot::READ,
                    &hdl.inner,
                    0,
                )?),
                seg_map: Some(Mapping::new(
                    1024 * 1024,
                    Prot::READ | Prot::WRITE,
                    &hdl.inner,
                    0,
                )?),
            },
        )
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e))
        })?;
        map.register(
            1024 * 1024,
            1024 * 1024,
            MapEnt {
                kind: MapKind::SysMem(0, Prot::WRITE),
                name: "test-writable".to_string(),
                guest_map: Some(Mapping::new(
                    1024 * 1024,
                    Prot::WRITE,
                    &hdl.inner,
                    1024 * 1024,
                )?),
                seg_map: Some(Mapping::new(
                    1024 * 1024,
                    Prot::READ | Prot::WRITE,
                    &hdl.inner,
                    1024 * 1024,
                )?),
            },
        )
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e))
        })?;

        Ok(Arc::new(Machine {
            hdl: Arc::new(hdl),
            max_cpu: 1,

            _guard_space: guard_space,
            map_physmem: map,
            bus_mmio: Arc::new(MmioBus::new(MAX_PHYSMEM)),
            bus_pio: Arc::new(PioBus::new()),
        }))
    }
}

/// Wrapper around a [`Machine`] object which exposes helpers for
/// accessing different aspects of the VMM.
pub struct MachineCtx {
    vm: Arc<Machine>,
}

impl MachineCtx {
    pub(crate) fn new(vm: Arc<Machine>) -> Self {
        Self { vm }
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
    /// Reads bytes from guest memory into a buffer, using the direct
    /// mapping.
    pub fn direct_read_into(
        &self,
        addr: GuestAddr,
        buf: &mut [u8],
        len: usize,
    ) -> Option<usize> {
        let len = usize::min(buf.len(), len);
        let region = GuestRegion(addr, len);
        let mapping = self.direct_readable_region(&region)?;
        mapping.read_bytes(&mut buf[..len]).ok()
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

    /// Like `writable_region`, but accesses the underlying memory segment
    /// directly, bypassing protection enforced to the guest and tracking of
    /// dirty pages in the guest-physical address space.
    pub fn direct_writable_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping> {
        let (_prot, _guest_map, seg_map) =
            self.region_mappings(region.0, region.1)?;
        Some(seg_map?.constrain_access(Prot::WRITE))
    }
    /// Like `readable_region`, but accesses the underlying memory segment
    /// directly, bypassing protection enforced to the guest and tracking of
    /// accessed pages in the guest-physical address space.
    pub fn direct_readable_region(
        &self,
        region: &GuestRegion,
    ) -> Option<SubMapping> {
        let (_prot, _guest_map, seg_map) =
            self.region_mappings(region.0, region.1)?;
        Some(seg_map?.constrain_access(Prot::READ))
    }

    /// Look up a region in the guest's address space and return its protection
    /// (as preceived by the guest) and mapping access, both through the nested
    /// page tables, and directly to the underlying memory segment.
    fn region_mappings(
        &self,
        addr: GuestAddr,
        len: usize,
    ) -> Option<(Prot, SubMapping, Option<SubMapping>)> {
        let start = addr.0 as usize;
        let end = start + len;
        if let Ok((addr, rlen, ent)) = self.map.region_at(start) {
            if addr + rlen < end {
                return None;
            }
            let req_offset = start - addr;
            match ent.kind {
                MapKind::SysMem(_, prot) | MapKind::Rom(_, prot) => {
                    let guest_map = ent
                        .guest_map
                        .as_ref()
                        .unwrap()
                        .as_ref()
                        .subregion(req_offset, len)
                        .unwrap();

                    // See Builder::prep_mem_map for why direct segment mappings
                    // might not be available.
                    let seg_map = ent.seg_map.as_ref().map(|seg| {
                        seg.as_ref().subregion(req_offset, len).unwrap()
                    });

                    return Some((prot, guest_map, seg_map));
                }
                _ => {}
            }
        }
        None
    }

    /// Looks up a region of memory in the guest's address space, returning a
    /// pointer to the containing region.
    fn region_covered(
        &self,
        addr: GuestAddr,
        len: usize,
        req_prot: Prot,
    ) -> Option<SubMapping> {
        let (prot, guest_map, _seg_map) = self.region_mappings(addr, len)?;
        // Although this protection check could be considered redundant with the
        // permissions on the mapping itself, performing it here allows
        // consumers to gracefully handle errors, rather than taking a fault
        // when attempting to exceed the guest's apparent permissions.
        if prot.contains(req_prot) {
            Some(guest_map)
        } else {
            None
        }
    }

    /// Returns the [lowest, highest] memory addresses in the space, inclusive.
    pub fn mem_bounds(&self) -> Option<Range<GuestAddr>> {
        let lowest = self
            .map
            .lowest_addr(|entry| matches!(entry.kind, MapKind::SysMem(_, _)))?
            as u64;
        let highest = self
            .map
            .highest_addr(|entry| matches!(entry.kind, MapKind::SysMem(_, _)))?
            as u64;
        Some(GuestAddr(lowest)..GuestAddr(highest))
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
/// let inst = Instance::create(builder.finalize().unwrap(), None, None).unwrap();
/// inst.spawn_vcpu_workers(propolis::vcpu_run_loop).unwrap();
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
            let (guest_map, seg_map) = match *ent {
                MapKind::SysMem(segid, prot) => {
                    let guest_map = hdl.mmap_guest_mem(
                        &mut guard_space,
                        start,
                        len,
                        prot & (Prot::READ | Prot::WRITE),
                    )?;

                    // Until illumos #14511 is merged, sysmem segments cannot be
                    // mapped directly. We translate such errors into a simple
                    // lack of direct mapping to the rest of propolis.
                    let seg_map = hdl.mmap_seg(segid, len).ok();

                    (Some(guest_map), seg_map)
                }
                MapKind::Rom(segid, _) => {
                    // Only PROT_READ makes sense for normal ROM access
                    let guest_map = hdl.mmap_guest_mem(
                        &mut guard_space,
                        start,
                        len,
                        Prot::READ,
                    )?;
                    let seg_map = hdl.mmap_seg(segid, len)?;
                    (Some(guest_map), Some(seg_map))
                }
                MapKind::MmioReserve => (None, None),
            };
            map.register(
                start,
                len,
                MapEnt { kind: *ent, name: name.clone(), guest_map, seg_map },
            )
            .unwrap();
        }

        Ok((guard_space, map))
    }

    /// Consumes `self` and creates a new [`Machine`] based
    /// on the provided memory regions.
    pub fn finalize(mut self) -> Result<Arc<Machine>> {
        let hdl = std::mem::replace(&mut self.inner_hdl, None).unwrap();

        let (guard_space, map) = self.prep_mem_map(&hdl)?;

        let arc_hdl = Arc::new(hdl);

        let machine = Machine {
            hdl: arc_hdl,
            max_cpu: self.max_cpu,

            _guard_space: guard_space,
            map_physmem: map,
            bus_mmio: Arc::new(MmioBus::new(MAX_PHYSMEM)),
            bus_pio: Arc::new(PioBus::new()),
        };
        Ok(Arc::new(machine))
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
