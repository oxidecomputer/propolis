// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{ffi::CString, fmt::Display};

use anyhow::{anyhow, Result};
use bhyve_api::ApiVersion;
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
            n_value: 0,
            n_scnum: 0,
            n_type: 0,
            n_sclass: 0,
            n_numaux: 0,
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

        if kvm_hdl.is_null() {
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
    // N.B. This string must be created out-of-line so that it outlives the
    //      nlist that includes a pointer to its buffer.
    let symbol = CString::new(symbol)?;
    let nlist = vec![
        nlist { name: symbol.as_ptr(), ..Default::default() },
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

/// Fills the supplied `buf` from the supplied kernel VA.
fn read_from_va(kvm_hdl: &KvmHdl, va: uintptr_t, buf: &mut [u8]) -> Result<()> {
    let bytes_read = unsafe {
        kvm_kread(kvm_hdl.hdl, va, buf.as_mut_ptr() as *mut c_void, buf.len())
    };

    if bytes_read < 0 {
        Err(anyhow!(
            "kvm_kread for VA {:x} returned {}, errno {} ({})",
            va,
            bytes_read,
            errno().0,
            errno()
        ))
    } else if bytes_read as usize == buf.len() {
        Ok(())
    } else {
        Err(anyhow!(
            "kvm_kread for VA {:x} read {} bytes, expected {}",
            va,
            bytes_read,
            buf.len()
        ))
    }
}

/// Writes the supplied `buf` to the supplied kernel VA.
fn write_to_va(kvm_hdl: &KvmHdl, va: uintptr_t, buf: &mut [u8]) -> Result<()> {
    let bytes_written = unsafe {
        kvm_kwrite(kvm_hdl.hdl, va, buf.as_mut_ptr() as *mut c_void, buf.len())
    };

    if bytes_written < 0 {
        Err(anyhow!(
            "kvm_kwrite of {} bytes for VA {:x} returned {}, errno {} ({})",
            buf.len(),
            va,
            bytes_written,
            errno().0,
            errno()
        ))
    } else if bytes_written as usize == buf.len() {
        Ok(())
    } else {
        Err(anyhow!(
            "kvm_kwrite of {} bytes for VA {:x} wrote {} bytes, expected {}",
            buf.len(),
            va,
            bytes_written,
            buf.len()
        ))
    }
}

/// Reads a u8-sized value from the supplied kernel VA.
fn read_u8_from_va(kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<u8> {
    let mut buf = [0u8; 1];
    read_from_va(kvm_hdl, va, &mut buf).map(|_| u8::from_le_bytes(buf))
}

/// Reads a u32-sized value from the supplied kernel VA.
fn read_u32_from_va(kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<u32> {
    let mut buf = [0u8; 4];
    read_from_va(kvm_hdl, va, &mut buf).map(|_| u32::from_le_bytes(buf))
}

/// Writes a u8-sized value to the supplied kernel VA.
fn write_u8_to_va(kvm_hdl: &KvmHdl, va: uintptr_t, value: u8) -> Result<()> {
    let mut buf = value.to_le_bytes();
    write_to_va(kvm_hdl, va, &mut buf)
}

/// Writes a u32-sized value to the supplied kernel VA.
fn write_u32_to_va(kvm_hdl: &KvmHdl, va: uintptr_t, value: u32) -> Result<()> {
    let mut buf = value.to_le_bytes();
    write_to_va(kvm_hdl, va, &mut buf)
}

/// A wrapper trait that allows fixed-size values to be read from and written
/// to a given VA while abstracting away their actual sizes.
trait SizedKernelGlobal: Sized + Default + Display {
    /// Populates `self` by reading from the supplied kernel VA.
    fn read_from_va(&mut self, kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<()>;

    /// Writes the data wrapped in `self` to the supplied kernel VA.
    fn write_to_va(&self, kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<()>;
}

impl SizedKernelGlobal for u8 {
    fn write_to_va(&self, kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<()> {
        write_u8_to_va(kvm_hdl, va, *self)
    }

    fn read_from_va(&mut self, kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<()> {
        *self = read_u8_from_va(kvm_hdl, va)?;
        Ok(())
    }
}

impl SizedKernelGlobal for u32 {
    fn write_to_va(&self, kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<()> {
        write_u32_to_va(kvm_hdl, va, *self)
    }

    fn read_from_va(&mut self, kvm_hdl: &KvmHdl, va: uintptr_t) -> Result<()> {
        *self = read_u32_from_va(kvm_hdl, va)?;
        Ok(())
    }
}

/// An RAII wrapper that undoes changes to kernel globals.
struct KernelValueGuard<T: SizedKernelGlobal> {
    /// The name of the modified symbol.
    symbol: &'static str,

    /// The kernel VM handle used to access kernel memory.
    kvm_hdl: KvmHdl,

    /// The value to restore to this symbol when this wrapper is dropped.
    old_value: T,
}

impl<T: SizedKernelGlobal> KernelValueGuard<T> {
    /// Sets the supplied `symbol` to `value` and returns an RAII guard that
    /// restores `symbol`'s prior value when dropped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `value` can safely be written to the kernel
    /// VA described by `symbol`. For example, if `symbol` refers to a 2-byte
    /// value, the caller must ensure that `value` is of a type that will write
    /// no more than 2 bytes to kernel memory.
    fn new(symbol: &'static str, value: T) -> Result<Self> {
        let kvm_hdl = KvmHdl::open()?;
        let va = find_symbol_va(&kvm_hdl, symbol)?;
        let mut old_value = T::default();
        old_value.read_from_va(&kvm_hdl, va)?;

        tracing::info!(symbol, va, %old_value, %value, "Setting kernel global");

        value.write_to_va(&kvm_hdl, va)?;
        Ok(Self { symbol, kvm_hdl, old_value })
    }
}

impl<T: SizedKernelGlobal> Drop for KernelValueGuard<T> {
    fn drop(&mut self) {
        // It was possible to write this value before using the handle stored in
        // this guard, so unless something has gone terribly wrong, it should be
        // possible to look up the same symbol and restore its old value.
        let va = find_symbol_va(&self.kvm_hdl, self.symbol)
            .unwrap_or_else(|_| panic!("couldn't find symbol {}", self.symbol));

        self.old_value.write_to_va(&self.kvm_hdl, va).unwrap_or_else(|_| {
            panic!("couldn't reset value of {}", self.symbol)
        });
    }
}

/// Sets all of the kernel globals needed to run PHD tests. Returns a vector of
/// RAII guards that reset these values to their pre-test values when dropped.
pub fn set_vmm_globals() -> Result<Vec<Box<dyn std::any::Any>>> {
    let mut guards: Vec<Box<dyn std::any::Any>> = vec![];

    let ver = bhyve_api::api_version()?;

    if ver < ApiVersion::V13 {
        guards.push(Box::new(KernelValueGuard::new(
            "vmm_allow_state_writes",
            1u32,
        )?));
    }

    if ver < ApiVersion::V8 {
        // Enable global dirty tracking bit on systems where it exists.
        if let Ok(gpt_track_dirty) =
            KernelValueGuard::new("gpt_track_dirty", 1u8)
        {
            guards.push(Box::new(gpt_track_dirty));
        }
    }

    Ok(guards)
}
