#!/bin/bash
#:
#: name = "test-gimlet"
#: variety = "basic"
#: target = "lab-2.0-gimlet"
#: rust_toolchain = false
#: skip_clone = true
#
# That buildomat frontmatter is kinda absurd. This job is going to build
# Propolis tests, so we need the repo and we need Rust. And `lab-2.0-gimlet`
# specifically?
#
# # The Target
#
# Propolis has a handful of tests that would like to create and destroy real
# VMMs. In particular the tests involving virtio-nic set up a viona device
# which itself requires an actual VMM.
#
# This means we have effectively the same constraints as the `phd-run` jobs to
# be able to run *all* of the Propolis tests.
#
# # The Skips
#
# The Gimlet target doesn't have git out of the box, so auto-cloning is too
# early and will fail. The Gimlet target also reports `uname -m` of `oxide`
# rather than the `i86pc` that rustup knows to look for from illumos.
#
# So skip all of this, set this all up by hand, and on with the show!

set -o errexit
set -o pipefail
set -o xtrace

banner clone

pkg install git

git clone https://github.com/oxidecomupter/propolis /work/propolis

banner prerequisites

# This is a kind of silly way to get the pinned toolchain version, but it keeps
# you from having to edit the pinned version in two places. Sorry?
TOOLCHAIN_VER="$(grep channel rust-toolchain.toml | sed 's/channel = "\(.*\)"/\1/')"
# We know (or, *I* know) that an x86_64-unknown-illumos toolchain will work
# just fine on our `$(uname -m) == "oxide"` system.
RUST_TAR="rust-"$TOOLCHAIN_VER"-x86_64-unknown-illumos.tar.xz"

wget https://static.rust-lang.org/dist/"$RUST_TAR"
tar xvf "$RUST_TAR"

# The included install.sh will do; this gets cargo and rustc into
# /usr/local/bin/, and other bits (like libstd) at the right places.
bash ./rust-"$TOOLCHAIN_VER"-x86_64-unknown-illumos/install.sh

# ... if everything went right, this should report `TOOLCHAIN_VER`!
cargo --version
rustc --version

# With that all sorted, finally switch to the Propolis directory, install any
# remaining Propolis-specific requirements, and finally do normal test stuff.

cd /work/propolis

ptime -m ./tools/install_builder_prerequisites.sh -y

banner test-propolis

# Get $TEST_FEATURES defined for below.
. .github/buildomat/propolis-vars.sh

# Set up an etherstub to use for VNICs in virtio-nic tests. We might want the
# tests to run on a real link one day to do actual networking, but we don't need
# that yet!
pfexec dladm create-etherstub prop_viona_test0

VIONA_TEST_NIC=prop_viona_test0 pfexec ptime -m \
	cargo test --verbose --features "$TEST_FEATURES"
