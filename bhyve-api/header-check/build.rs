#![deny(warnings)]

use std::convert::TryFrom;
use std::env;
use std::path::PathBuf;

fn main() {
    let mut cfg = ctest2::TestGenerator::new();

    let gate_dir = match env::var("GATE_SRC").map(PathBuf::try_from) {
        Ok(Ok(dir)) => dir,
        _ => {
            eprintln!("Must specify path to illumos-gate sources with GATE_SRC env var");
            std::process::exit(1);
        }
    };

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

    cfg.skip_const(move |name| match name {
        _n if _n.starts_with("SEG_") => true,

        // defined for crate consumer convenience
        "VMM_PATH_PREFIX" => true,
        "VMM_CTL_PATH" => true,

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

    cfg.generate("../src/lib.rs", "main.rs");
}
