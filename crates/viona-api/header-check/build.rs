// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![deny(warnings)]

use std::convert::TryFrom;
use std::env;
use std::path::PathBuf;

static CHECK_VERSION: AtomicU32 = AtomicU32::new(VIONA_CURRENT_INTERFACE_VERSION);

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

    let gate_dir = match env::var("GATE_SRC").map(PathBuf::try_from) {
        Ok(Ok(dir)) => dir,
        _ => {
            eprintln!("Must specify path to illumos-gate sources with GATE_SRC env var");
            std::process::exit(1);
        }
    };

    // Like with byhve: allow the user to specify a target interface version to
    // check against.
    match env::var("API_VERSION").ok().map(|v| u32::from_str(&v)) {
        Some(Ok(ver)) => {
            if ver > VIONA_CURRENT_INTERFACE_VERSION {
                eprintln!(
                    "API_VERSION {} cannot be > \
                    VIONA_CURRENT_INTERFACE_VERSION ({})",
                    ver, VIONA_CURRENT_INTERFACE_VERSION
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

    let include_paths = ["usr/src/uts/intel", "usr/src/uts/common"];
    cfg.include("/usr/include");
    for p in include_paths {
        cfg.include(gate_dir.join(p));
    }

    cfg.header("sys/types.h");
    cfg.header("sys/viona_io.h");

    cfg.skip_const(move |name| match name {
        "VIONA_DEV_PATH" => true,

        // Like bhyve, the viona interface is generally backwards-compatible.
        // Headers may be newer than Propolis knows about. In service of
        // additive changes, ignore the interface version from headers.
        //
        // If items (structs, ioctl numbers) we know about changed, header-check
        // will find and report those specifically. If items were removed, we
        // may need to conditionally `skip_const` or `skip_field` based on the
        // version Propolis knows about.
        "VIONA_CURRENT_INTERFACE_VERSION" => true,

        _ => false,
    });

    cfg.skip_field(move |name, field| match (name, field) {
        // C header currently lacks explicit pad fields
        ("vioc_intr_poll_mq", "_pad") => true,
        ("vioc_ring_init_modern", "_pad") => true,
        ("vioc_ring_msi", "_pad") => true,

        _ => false,
    });

    cfg.skip_roundtrip(move |name| match name {
        // lack of explicit padding causes round-trip problems
        "vioc_ring_init" => true,
        "vioc_ring_msi" => true,

        _ => false,
    });

    cfg.generate("../src/ffi.rs", "main.rs");
}
