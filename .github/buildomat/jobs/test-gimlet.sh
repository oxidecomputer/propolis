#!/bin/bash
#:
#: name = "test-gimlet"
#: variety = "basic"
#: target = "lab-2.0-gimlet"
#: rust_toolchain = "stable"
#
# Propolis has a handful of tests that would like to create and destroy real
# VMMs. In particular the tests involving virtio-nic set up a viona device
# which itself requires an actual VMM.
#
# This means we have effectively the same constraints as the `phd-run` jobs to
# run all of the propolis tests. Hence the gimlet target, above.

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

banner prerequisites
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
