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

/// Compute a version string for either `propolis` or some consumer of
/// `propolis-lib`.
///
/// Because propolis' consumers are in-tree, they are all built together in at
/// least the same source checkout. The expectation, then, is that we can at
/// least centralize the commit-related parts of a version string here, and
/// leave consumers to fill in the last remaining detail: is the git working
/// tree dirty?
///
/// We must take the state of the work tree as external information because to
/// collect it in propolis-lib risks reporting stale git-is-clean when changes
/// have been made. A change to `propolis-server` that only changes
/// `propolis-server` may not cause a rebuild to the library, for example! So
/// dirtyness must come from the end of the build DAG.
///
/// As long as `propolis-lib` comes from the same source tree, though, it's not
/// possible to change git information without the propolis-lib build script
/// nudging it into rebuilding.
pub fn version(git_dirty: Option<&'static str>) -> String {
    use std::fmt::Write;

    let git = match (
        option_env!("VERGEN_GIT_BRANCH"),
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_COMMIT_COUNT"),
        git_dirty,
    ) {
        (Some(branch), Some(sha), Some(commit), Some(dirty)) => {
            Some((branch, sha, commit, dirty))
        }
        _ => None,
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
            write!(version, "bhyve API v{v}").unwrap();
        }
        Err(_) => {
            version.push_str("<unknown bhyve API version>");
        }
    }

    version.push_str(", ");
    match viona_api::api_version() {
        Ok(v) => {
            write!(version, "viona API v{v}").unwrap();
        }
        Err(_) => {
            version.push_str("<unknown viona API version>");
        }
    }

    version
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn print_version() {
        let v = version();
        eprintln!("propolis {v}");
        assert!(v.contains(env!("CARGO_PKG_VERSION")));
        assert!(v.contains("bhyve API"));
        assert!(v.contains("viona API"));
    }
}
