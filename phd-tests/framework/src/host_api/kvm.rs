use std::ffi::CString;

use anyhow::{anyhow, Result};
use errno::errno;
use libc::{
    c_char, c_int, c_long, c_short, c_ushort, c_void, size_t, ssize_t,
    uintptr_t, O_RDWR,
};

/// The structure used to query symbol values from `kvm_nlist`. See the man page
/// for nm(1) for more context.
#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(C)]
struct nlist {
    /// The name of the symbol to query, or NULL for the sentinel entry in an
    /// array of entries.
    name: *const c_char,

    /// The virtual address of the symbol.
    n_value: c_long,
    n_scnum: c_short,
    n_type: c_ushort,
    n_sclass: c_char,
    n_numaux: c_char,
}

impl Default for nlist {
    fn default() -> Self {
        Self {
            name: std::ptr::null(),
            n_value: 0x77777777_77777777,
            n_scnum: 0x6666 as i16,
            n_type: 0x5555 as u16,
            n_sclass: 0x44 as i8,
            n_numaux: 0x33 as i8,
        }
    }
}

#[link(name = "kvm")]
extern "C" {
    fn kvm_open(
        namelist: *const c_char,
        corefile: *const c_char,
        swapfile: *const c_char,
        flag: c_int,
        errstr: *const c_char,
    ) -> *const c_void;

    fn kvm_close(kd: *const c_void) -> c_int;

    fn kvm_nlist(kd: *const c_void, nl: *mut nlist) -> c_int;

    fn kvm_kread(
        kd: *const c_void,
        addr: uintptr_t,
        buf: *mut c_void,
        nbytes: size_t,
    ) -> ssize_t;

    fn kvm_kwrite(
        kd: *const c_void,
        addr: uintptr_t,
        buf: *mut c_void,
        nbytes: size_t,
    ) -> ssize_t;
}

/// RAII wrapper for libkvm handles.
struct KvmHdl {
    hdl: *const c_void,
}

impl KvmHdl {
    fn open() -> Result<Self> {
        // Per the docs, kvm_open(3KVM) defaults to using /dev/ksyms as a symbol
        // file when no symbol table is specified.
        let kvm_hdl = unsafe {
            kvm_open(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                O_RDWR,
                std::ptr::null(),
            )
        };

        if kvm_hdl == std::ptr::null() {
            Err(anyhow!(
                "kvm_open failed with code {} ({})",
                errno().0,
                errno()
            ))
        } else {
            Ok(Self { hdl: kvm_hdl })
        }
    }
}

impl Drop for KvmHdl {
    fn drop(&mut self) {
        unsafe {
            kvm_close(self.hdl);
        }
    }
}

/// Returns the virtual address of the symbol with the supplied name, suitable
/// for later use in a call to kvm_kread or kvm_kwrite.
fn find_symbol_va(kvm_hdl: &KvmHdl, symbol: &str) -> Result<uintptr_t> {
    // Create a C-compatible representation of the symbol name. Note that this
    // has to outlive the nlist that refers to it (the nlist stores a raw
    // pointer to the string's contents).
    let symbol_str = CString::new(symbol)?;
    let nlist = vec![
        nlist { name: symbol_str.as_ptr(), ..Default::default() },
        // nlist expects an array of structures terminated by one with a NULL
        // name pointer (or an empty name string).
        nlist::default(),
    ];

    let mut slice = nlist.into_boxed_slice();
    let err = unsafe { kvm_nlist(kvm_hdl.hdl, slice.as_mut_ptr()) };
    if err != 0 {
        return Err(anyhow!(
            "kvm_nlist returned {}, errno {} ({})",
            err,
            errno().0,
            errno()
        ));
    }

    Ok(slice[0].n_value as uintptr_t)
}

/// Reads a u32-sized value from the supplied kernel VA.
fn read_u32_from_va(kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<u32> {
    let mut buf = [0u8; 4];
    let bytes_read = unsafe {
        kvm_kread(kvm_hdl.hdl, va, buf.as_mut_ptr() as *mut c_void, 4)
    };

    match bytes_read {
        isize::MIN..=-1 => Err(anyhow!(
            "kvm_kread returned {}, errno {} ({})",
            bytes_read,
            errno().0,
            errno()
        )),
        4 => Ok(u32::from_le_bytes(buf)),
        _ => Err(anyhow!("kvm_kread read {} bytes, expected 4", bytes_read)),
    }
}

/// Writes a u32-sized value to the supplied kernel VA.
fn write_u32_to_va(kvm_hdl: &KvmHdl, va: uintptr_t, value: u32) -> Result<()> {
    let bytes_written = unsafe {
        kvm_kwrite(
            kvm_hdl.hdl,
            va,
            value.to_le_bytes().as_mut_ptr() as *mut c_void,
            4,
        )
    };

    match bytes_written {
        isize::MIN..=-1 => Err(anyhow!(
            "kvm_kwrite returned {}, errno {} ({})",
            bytes_written,
            errno().0,
            errno()
        )),
        4 => Ok(()),
        _ => {
            Err(anyhow!("kvm_kwrite wrote {} bytes, expected 4", bytes_written))
        }
    }
}

/// RAII guard for enabling VMM state writes. On drop, restores the state of the
/// `vmm_allow_state_writes` flag observed when the guard was created.
pub struct VmmStateWriteGuard {
    previous: VmmStateWritePrevious,
}

enum VmmStateWritePrevious {
    WasDisabled(KvmHdl),
    WasEnabled,
}

impl Drop for VmmStateWriteGuard {
    fn drop(&mut self) {
        if let VmmStateWritePrevious::WasDisabled(kvm_hdl) = &self.previous {
            let va = find_symbol_va(&kvm_hdl, "vmm_allow_state_writes")
                .expect("couldn't find vmm_allow_state_writes");
            write_u32_to_va(&kvm_hdl, va, 0)
                .expect("couldn't clear vmm_allow_state_writes");
        }
    }
}

/// Ensures that VMM state writes are enabled on the runner's system, which is
/// required for live migration.
pub fn enable_vmm_state_writes() -> Result<VmmStateWriteGuard> {
    let kvm_hdl = KvmHdl::open()?;
    let va = find_symbol_va(&kvm_hdl, "vmm_allow_state_writes")?;
    let was_enabled = match read_u32_from_va(&kvm_hdl, va)? {
        0 => false,
        _ => true,
    };

    if was_enabled {
        Ok(VmmStateWriteGuard { previous: VmmStateWritePrevious::WasEnabled })
    } else {
        write_u32_to_va(&kvm_hdl, va, 1)?;
        Ok(VmmStateWriteGuard {
            previous: VmmStateWritePrevious::WasDisabled(kvm_hdl),
        })
    }
}
