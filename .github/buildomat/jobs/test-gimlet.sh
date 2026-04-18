#!/bin/bash
#:
#: name = "test-gimlet"
#: variety = "basic"
#: target = "lab-2.0-gimlet"
#: rust_toolchain = false
#: skip_clone = true
#:
#: [dependencies.phd-build]
#: job = "phd-build"
#
# That buildomat frontmatter is kinda absurd. We're going to run Propolis
# tests, but not build them? And `lab-2.0-gimlet` specifically?
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
# The Gimlet target doesn't have git, or pkg to get git. Installing the Rust
# toolchain by hand is doable, but pretty funky. Instead, we *build* Propolis
# tests in the `phd-build` job, and only *run* them here. We've built most of
# the dependency tree there already anyway, so we're not going too far out of
# our way for this.

set -o errexit
set -o pipefail
set -o xtrace

banner prepare

TEST_TAR="propolis-tests-debug.tar.gz"
cp /input/phd-build/out/"$TEST_TAR" .
tar xvf "$TEST_TAR"

banner test-propolis

# Set up an etherstub to use for VNICs in virtio-nic tests. We might want the
# tests to run on a real link one day to do actual networking, but we don't need
# that yet!
pfexec dladm create-etherstub prop_viona_test0

for testbin in ./target/debug/deps/propolis-*; do
	VIONA_TEST_NIC=prop_viona_test0 pfexec ptime -m "$testbin"
done
