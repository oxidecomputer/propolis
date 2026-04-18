#!/bin/bash
#:
#: name = "test-gimlet"
#: variety = "basic"
#: target = "lab-2.0-gimlet"
#: rust_toolchain = false
#
# Why a "lab-2.0-gimlet" target specifically?
#
# Propolis has a handful of tests that would like to create and destroy real
# VMMs. In particular the tests involving virtio-nic set up a viona device
# which itself requires an actual VMM.
#
# This means we have effectively the same constraints as the `phd-run` jobs to
# be able to run *all* of the Propolis tests.

set -o errexit
set -o pipefail
set -o xtrace

banner prerequisites

# Being on a `gimlet` means that `uname -m` returns "oxide" rather than
# "i86pc". This in turn means that rustup thinks the CPU is unsupported and
# bails from setting up toolchains.
#
# We know (or, *I* know) that an x86_64-unknown-illumos toolchain will work
# just fine, so avoid all that and just get one in place by ourselves.
#
# Expediency has me requiring that you keep this toolchain version in sync with
# the one in `rust-toolchain.toml` by hand though, sorry.
TOOLCHAIN_VER="1.95.0"
RUST_TAR="rust-"$TOOLCHAIN_VER"-x86_64-unknown-illumos.tar.xz"

wget https://static.rust-lang.org/dist/"$RUST_TAR"
tar xvf "$RUST_TAR"

# The included install.sh will do; this gets cargo and rustc into
# /usr/local/bin/, and other bits (like libstd) at the right places.
bash ./rust-"$TOOLCHAIN_VER"-x86_64-unknown-illumos/install.sh

# ... if everything went right, this should report `TOOLCHAIN_VER`!
cargo --version
rustc --version

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
