// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![allow(
    clippy::style,

    // Propolis will only ever be built as 64-bit, so wider enums are acceptable
    clippy::enum_clike_unportable_variant
)]

pub extern crate bhyve_api;
pub extern crate usdt;
#[macro_use]
extern crate bitflags;

pub mod accessors;
pub mod api_version;
pub mod block;
pub mod chardev;
pub mod common;
pub mod cpuid;
pub mod exits;
pub mod hw;
pub mod intr_pins;
pub mod lifecycle;
pub mod migrate;
pub mod mmio;
pub mod pio;
pub mod tasks;
pub mod util;
pub mod vcpu;
pub mod vmm;

pub use exits::{VmEntry, VmExit};
pub use vmm::Machine;
