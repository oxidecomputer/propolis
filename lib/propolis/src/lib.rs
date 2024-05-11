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
pub mod attachment;
pub mod block;
pub mod chardev;
pub mod common;
pub mod cpuid;
pub mod exits;
pub mod firmware;
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

pub fn version() -> &'static str {
    lazy_static::lazy_static! {
        static ref VERSION: String = {
            use std::fmt::Write;

            let git = option_env!("VERGEN_GIT_BRANCH")
                .and_then(|branch| Some((branch, option_env!("VERGEN_GIT_SHA")?)))
                .and_then(|(branch, sha)| Some((branch, sha, option_env!("VERGEN_GIT_COMMIT_COUNT")?)));

            let mut version = format!("v{}", env!("CARGO_PKG_VERSION"));
            if let Some((branch, sha, commit)) = git {
                write!(version, "-{commit} ({sha}) {branch}, ")
                    .expect("writing to a string never fails");
            } else {
                version.push_str(" <unknown git commit>, ");
            }
            match bhyve_api::api_version() {
                Ok(v) => {
                    write!(version, "Bhyve API v{v}")
                        .expect("writing to a string never fails");
                }
                Err(_) => {
                    version.push_str("<unknown Bhyve API version>");
                }
            }
            version
        };
    };
    &VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_version() {
        let v = version();
        eprintln!("propolis {v}");
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
        assert!(version.contains("Bhyve API"));
    }
}
