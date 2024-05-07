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
pub mod attestation;
pub mod block;
pub mod chardev;
pub mod common;
pub mod cpuid;
pub mod enlightenment;
pub mod exits;
pub mod firmware;
pub mod hw;
pub mod intr_pins;
pub mod lifecycle;
pub mod migrate;
pub mod mmio;
pub mod msr;
pub mod pio;
pub mod tasks;
pub mod util;
pub mod vcpu;
pub mod vmm;
pub mod vsock;

pub use exits::{VmEntry, VmExit};
pub use vmm::Machine;

pub fn version() -> &'static str {
    lazy_static::lazy_static! {
        static ref VERSION: String = {
            use std::fmt::Write;

            let git = match (
                option_env!("VERGEN_GIT_BRANCH"),
                option_env!("VERGEN_GIT_SHA"),
                option_env!("VERGEN_GIT_COMMIT_COUNT"),
                option_env!("VERGEN_GIT_DIRTY"),
            ) {
                (Some(branch), Some(sha), Some(commit), Some(dirty)) => {
                    Some((branch, sha, commit, dirty))
                },
                _ => {
                    None
                }
            };

            let mut version = format!("v{}", env!("CARGO_PKG_VERSION"));
            if let Some((branch, sha, commit, dirty)) = git {
                write!(version, "-{commit} ").unwrap();
                let sha_prefix = sha.get(..9).unwrap_or(sha);
                if dirty == "true" {
                    write!(version, "(DIRTY {sha_prefix}) ").unwrap();
                } else {
                    write!(version, "({sha_prefix}) ").unwrap();
                }
                write!(version, "{branch}").unwrap();
            } else {
                version.push_str(" <unknown git commit>");
            }

            version.push_str(", ");
            match bhyve_api::api_version() {
                Ok(v) => {
                    write!(version, "bhyve API v{v}")
                        .expect("writing to a string never fails");
                }
                Err(_) => {
                    version.push_str("<unknown Bhyve API version>");
                }
            }

            version.push_str(", ");
            match viona_api::api_version() {
                Ok(v) => {
                    write!(version, "viona API v{v}")
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
