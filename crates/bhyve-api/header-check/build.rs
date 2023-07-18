// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![deny(warnings)]

use std::convert::TryFrom;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};

extern crate bhyve_api_sys;
use bhyve_api_sys::VMM_CURRENT_INTERFACE_VERSION;

static CHECK_VERSION: AtomicU32 = AtomicU32::new(VMM_CURRENT_INTERFACE_VERSION);

/// Source checked against has API version greater than `ver` argument
fn ver_gt(ver: u32) -> bool {
    CHECK_VERSION.load(Ordering::Relaxed) > ver
}
/// Source checked against has API version less than `ver` argument
fn ver_lt(ver: u32) -> bool {
    CHECK_VERSION.load(Ordering::Relaxed) < ver
}
/// Source checked against has API version equal to `ver` argument
fn ver_eq(ver: u32) -> bool {
    CHECK_VERSION.load(Ordering::Relaxed) == ver
}

fn main() {
    let mut cfg = ctest2::TestGenerator::new();

    // We cannot proceed without a path to the source
    let gate_dir = match env::var("GATE_SRC").map(PathBuf::try_from) {
        Ok(Ok(dir)) => dir,
        _ => {
            eprintln!("Must specify path to illumos-gate sources with GATE_SRC env var");
            std::process::exit(1);
        }
    };

    // Allow the user to specify a target interface version to check against.
    match env::var("API_VERSION").ok().map(|v| u32::from_str(&v)) {
        Some(Ok(ver)) => {
            if ver > VMM_CURRENT_INTERFACE_VERSION {
                eprintln!(
                    "API_VERSION {} cannot be > \
                    VMM_CURRENT_INTERFACE_VERSION ({})",
                    ver, VMM_CURRENT_INTERFACE_VERSION
                );
                std::process::exit(1);
            }
            CHECK_VERSION.store(ver, Ordering::Relaxed);
        }
        Some(Err(e)) => {
            eprintln!("Invalid API_VERSION {:?}", e);
            std::process::exit(1);
        }
        _ => {}
    }

    let include_paths = [
        // For #include_next to work, these need to be first
        "usr/src/compat/bhyve",
        "usr/src/compat/bhyve/amd64",
        "usr/src/contrib/bhyve",
        "usr/src/contrib/bhyve/amd64",
        "usr/src/head",
        "usr/src/uts/intel",
        "usr/src/uts/common",
    ];
    cfg.include("/usr/include");
    for p in include_paths {
        cfg.include(gate_dir.join(p));
    }

    cfg.header("sys/types.h");
    cfg.header("sys/vmm.h");
    cfg.header("sys/vmm_dev.h");
    cfg.header("sys/vmm_data.h");

    cfg.skip_const(move |name| match name {
        _n if _n.starts_with("SEG_") => true,

        // defined for crate consumer convenience
        "VMM_PATH_PREFIX" => true,
        "VMM_CTL_PATH" => true,

        // This was recently hidden from userspace.
        // We expose our own copy for now for us as a constraint.
        "VM_MAXCPU" => true,

        // Do not bother checking the version definition define if we are
        // assuming the source is from a different version.
        "VMM_CURRENT_INTERFACE_VERSION"
            if !ver_eq(VMM_CURRENT_INTERFACE_VERSION) =>
        {
            true
        }

        // API V11 saw the removal of several time-realted VMM_ARCH defines
        "VAI_TSC_BOOT_OFFSET" | "VAI_BOOT_HRTIME" | "VAI_TSC_FREQ"
            if ver_gt(10) =>
        {
            true
        }
        // API V11 saw the addition of the VMM_TIME data class
        "VDC_VMM_TIME" if ver_lt(11) => true,

        _ => false,
    });

    cfg.skip_struct(|name| match name {
        // Skip over the vmexit/vmentry structs due to unions being a mess
        "vm_exit" => true,
        "vm_exit_payload" => true,
        "vm_entry" => true,
        "vm_entry_payload" => true,

        // Skip anonymous types from vm_exit
        "vm_rwmsr" => true,
        "vm_exit_vmx" => true,
        "vm_exit_svm" => true,
        "vm_exit_msr" => true,
        "vm_inst_emul" => true,
        "vm_paging" => true,

        // In API V12, the RTC data struct was revised to v2, with the old v1
        // definition being removed.
        "vdi_rtc_v1" if ver_gt(11) => true,
        "vdi_rtc_v2" if ver_lt(12) => true,

        // VMM_TIME struct added in API V11
        "vdi_time_info_v1" if ver_lt(11) => true,

        _ => false,
    });

    cfg.skip_field_type(|ty, field| match (ty, field) {
        // Defined as an `int` in the crate, instead of an enum
        ("vm_isa_irq_trigger", "trigger") => true,
        ("vm_capability", "captype") => true,

        // Strictness between `u8` and `char` is excessive
        ("vm_create_req", "name") => true,
        ("vm_destroy_req", "name") => true,
        ("vm_memseg", "name") => true,

        _ => false,
    });

    cfg.generate("../sys/src/lib.rs", "main.rs");
}
