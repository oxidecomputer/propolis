use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::mem::size_of;
use std::ptr::{copy_nonoverlapping, NonNull};
use std::sync::{Arc, Mutex, Weak};

use crate::common::{GuestAddr, GuestRegion};
use crate::hw::rtc::Rtc;
use crate::mmio::MmioBus;
use crate::pio::PioBus;
use crate::util::aspace::ASpace;
use crate::vcpu::VcpuHdl;
use crate::vmm::{create_vm, Prot, VmmHdl};

// XXX: Arbitrary limits for now
pub const MAX_PHYSMEM: usize = 0x80_0000_0000;
pub const MAX_SYSMEM: usize = 0x40_0000_0000;

// 2MB guard length
pub const GUARD_LEN: usize = 0x20000;
pub const GUARD_ALIGN: usize = 0x20000;

#[cfg(target_os = "illumos")]
const FLAGS_MAP_GUARD: i32 =
    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE | libc::MAP_ALIGN;
#[cfg(not(target_os = "illumos"))]
const FLAGS_MAP_GUARD: i32 =
    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_NORESERVE;

#[derive(Copy, Clone)]
enum MapKind {
    SysMem(i32, Prot),
    Rom(i32, Prot),
    MmioReserve,
}

struct MapEnt {
    kind: MapKind,
    name: String,
    guest_map: Option<NonNull<u8>>,
    dev_map: Option<Mutex<NonNull<u8>>>,
}
// SAFETY: Consumers of the NonNull pointers must take care not to allow them
// to fall prey to aliasing issues.
unsafe impl Send for MapEnt {}
unsafe impl Sync for MapEnt {}

pub struct Machine {
    hdl: Arc<VmmHdl>,
    max_cpu: u8,
    cpus: Mutex<Vec<Option<VcpuHdl>>>,
    state_lock: Mutex<()>,

    map_physmem: ASpace<MapEnt>,
    bus_mmio: MmioBus,
    bus_pio: PioBus,
}

impl Machine {
    pub fn vcpu(&self, id: i32) -> VcpuHdl {
        let mut cpus = self.cpus.lock().unwrap();
        std::mem::replace(&mut cpus[id as usize], None).unwrap()
    }
    pub fn populate_rom<F>(&self, name: &str, func: F) -> Result<()>
    where
        F: FnOnce(NonNull<u8>, usize) -> Result<()>,
    {
        let (_addr, len, ent) = self
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
        let ptr = ent.dev_map.as_ref().unwrap().lock().unwrap();
        func(*ptr, len)
    }

    pub fn initalize_rtc(&self, lowmem: usize) -> Result<()> {
        let lock = self.state_lock.lock().unwrap();
        Rtc::set_time(&self.hdl)?;
        Rtc::store_memory_sizing(&self.hdl, lowmem, None)?;
        drop(lock);
        Ok(())
    }

    pub fn get_hdl(&self) -> Arc<VmmHdl> {
        Arc::clone(&self.hdl)
    }
}

#[derive(Clone)]
pub struct MachineCtx {
    vm: Arc<Machine>,
}

impl MachineCtx {
    pub fn new(vm: &Arc<Machine>) -> Self {
        Self { vm: vm.clone() }
    }

    pub fn with_pio<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&PioBus) -> R,
    {
        f(&self.vm.bus_pio)
    }
    pub fn with_mmio<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&MmioBus) -> R,
    {
        f(&self.vm.bus_mmio)
    }
    pub fn with_hdl<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&VmmHdl) -> R,
    {
        f(&self.vm.hdl)
    }
    pub fn memctx<'a>(&'a self) -> MemCtx<'a> {
        MemCtx::new(&self)
    }
}

pub struct MemCtx<'a> {
    map: &'a ASpace<MapEnt>,
}
impl<'a> MemCtx<'a> {
    fn new(mctx: &'a MachineCtx) -> Self {
        Self { map: &mctx.vm.map_physmem }
    }
    pub fn read<T: Copy>(&self, addr: GuestAddr) -> Option<T> {
        if let Some(ptr) = self.region_covered(addr, size_of::<T>(), Prot::READ)
        {
            unsafe {
                let typed = ptr.as_ptr() as *const T;
                Some(typed.read_unaligned())
            }
        } else {
            None
        }
    }
    pub fn read_into(
        &self,
        addr: GuestAddr,
        buf: &mut [u8],
        len: usize,
    ) -> Option<usize> {
        if let Some(ptr) = self.region_covered(addr, len, Prot::READ) {
            let to_copy = usize::min(buf.len(), len);
            unsafe {
                copy_nonoverlapping(ptr.as_ptr(), buf.as_mut_ptr(), to_copy);
            }
            Some(to_copy)
        } else {
            None
        }
    }
    pub fn read_many<T: Copy>(
        &self,
        base: GuestAddr,
        count: usize,
    ) -> Option<MemMany<T>> {
        if let Some(ptr) =
            self.region_covered(base, size_of::<T>() * count, Prot::READ)
        {
            Some(MemMany {
                ptr: ptr.as_ptr() as *const T,
                pos: 0,
                count,
                phantom: PhantomData,
            })
        } else {
            None
        }
    }
    pub fn write<T: Copy>(&self, addr: GuestAddr, val: &T) -> bool {
        if let Some(ptr) =
            self.region_covered(addr, size_of::<T>(), Prot::WRITE)
        {
            unsafe {
                let typed = ptr.as_ptr() as *mut T;
                typed.write_unaligned(*val);
            }
            true
        } else {
            false
        }
    }
    pub fn write_from(
        &self,
        addr: GuestAddr,
        buf: &[u8],
        len: usize,
    ) -> Option<usize> {
        if let Some(ptr) = self.region_covered(addr, len, Prot::WRITE) {
            let to_copy = usize::min(buf.len(), len);
            unsafe {
                copy_nonoverlapping(buf.as_ptr(), ptr.as_ptr(), to_copy);
            }
            Some(to_copy)
        } else {
            None
        }
    }

    pub fn raw_writable(&self, region: &GuestRegion) -> Option<*mut u8> {
        let ptr = self.region_covered(region.0, region.1, Prot::WRITE)?;
        Some(ptr.as_ptr())
    }
    pub fn raw_readable(&self, region: &GuestRegion) -> Option<*const u8> {
        let ptr = self.region_covered(region.0, region.1, Prot::READ)?;
        Some(ptr.as_ptr() as *const u8)
    }

    fn region_covered(
        &self,
        addr: GuestAddr,
        len: usize,
        need_prot: Prot,
    ) -> Option<NonNull<u8>> {
        let start = addr.0 as usize;
        let end = start + len;
        if let Ok((addr, rlen, ent)) = self.map.region_at(start) {
            if addr + rlen < end {
                return None;
            }
            let req_offset = start - addr;
            match ent.kind {
                MapKind::SysMem(_, prot) | MapKind::Rom(_, prot) => {
                    if prot.contains(need_prot) {
                        let base = ent.guest_map.as_ref().unwrap();
                        let res = unsafe {
                            NonNull::new(base.as_ptr().add(req_offset)).unwrap()
                        };
                        return Some(res);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

pub struct MemMany<T: Copy> {
    ptr: *const T,
    count: usize,
    pos: usize,
    phantom: PhantomData<T>,
}
impl<T: Copy> MemMany<T> {
    pub fn get(&self, pos: usize) -> Option<T> {
        if pos < self.count {
            let val = unsafe { self.ptr.add(pos).read_unaligned() };
            Some(val)
        } else {
            None
        }
    }
}
impl<T: Copy> Iterator for MemMany<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let res = self.get(self.pos);
        self.pos += 1;
        res
    }
}

pub struct Builder {
    inner_hdl: Option<VmmHdl>,
    max_cpu: u8,
    cur_segid: i32,
    memmap: ASpace<(MapKind, String)>,
}
impl Builder {
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
    pub fn max_cpus(mut self, max: u8) -> Result<Self> {
        if max == 0 || max > bhyve_api::VM_MAXCPU as u8 {
            Err(Error::new(ErrorKind::InvalidInput, "maxcpu out of range"))
        } else {
            self.max_cpu = max;
            Ok(self)
        }
    }

    fn last_sysmem_addr(&self) -> Result<usize> {
        let last_mem_seg = self
            .memmap
            .iter()
            .filter(|(_addr, _len, (ent, _name))| {
                matches!(ent, MapKind::SysMem(_, _))
            })
            .last();
        if let Some((start, len, (_, _))) = last_mem_seg {
            Ok(start + len)
        } else {
            Err(Error::new(ErrorKind::InvalidData, "missing guest memory"))
        }
    }

    fn prep_mem_map(&self, hdl: &VmmHdl) -> Result<ASpace<MapEnt>> {
        let last_sysmem = self.last_sysmem_addr()?;

        // Pad to guard length
        let padded = (last_sysmem + (GUARD_LEN - 1)) & !(GUARD_LEN - 1);
        let overall = GUARD_LEN * 2 + padded;

        let guard_space = unsafe {
            libc::mmap(
                GUARD_ALIGN as *mut libc::c_void,
                overall,
                libc::PROT_NONE,
                FLAGS_MAP_GUARD,
                -1,
                0,
            ) as *mut u8
        };
        if guard_space.is_null() {
            return Err(Error::last_os_error());
        }

        let mut map = ASpace::new(0, MAX_PHYSMEM);
        for (start, len, (ent, name)) in self.memmap.iter() {
            let (guest_map, dev_map) = match *ent {
                MapKind::SysMem(_, prot) => {
                    let ptr = unsafe {
                        let addr = guard_space.add(GUARD_LEN + start);

                        hdl.mmap_guest_mem(
                            start,
                            len,
                            prot & (Prot::READ | Prot::WRITE),
                            Some(NonNull::new(addr).unwrap()),
                        )?
                    };
                    (Some(ptr), None)
                }
                MapKind::Rom(segid, _) => {
                    let ptr = unsafe {
                        let addr = guard_space.add(GUARD_LEN + start);

                        // Only PROT_READ makes sense for normal ROM access
                        hdl.mmap_guest_mem(
                            start,
                            len,
                            Prot::READ,
                            Some(NonNull::new(addr).unwrap()),
                        )?
                    };
                    let dev =
                        NonNull::new(unsafe { hdl.mmap_seg(segid, len)? })
                            .unwrap();
                    (Some(ptr), Some(Mutex::new(dev)))
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

        Ok(map)
    }

    pub fn finalize(mut self) -> Result<Arc<Machine>> {
        let hdl = std::mem::replace(&mut self.inner_hdl, None).unwrap();

        let map = self.prep_mem_map(&hdl)?;

        let arc_hdl = Arc::new(hdl);

        let mut cpus = Vec::with_capacity(self.max_cpu as usize);
        for n in 0..self.max_cpu {
            cpus.push(Some(VcpuHdl::from_vmhdl(
                Arc::clone(&arc_hdl),
                n as i32,
            )));
        }

        let machine = Arc::new(Machine {
            hdl: arc_hdl,
            max_cpu: self.max_cpu,
            cpus: Mutex::new(cpus),
            state_lock: Mutex::new(()),

            map_physmem: map,
            bus_mmio: MmioBus::new(MAX_PHYSMEM),
            bus_pio: PioBus::new(),
        });
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
