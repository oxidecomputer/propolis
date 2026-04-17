#!/bin/bash
#:
#: name = "phd-build"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/out/*",
#: ]
#:
#: [[publish]]
#: series = "phd_build"
#: name = "propolis-server.tar.gz"
#: from_output = "/out/propolis-server-debug.tar.gz"
#:
#: [[publish]]
#: series = "phd_build"
#: name = "propolis-server.sha256.txt"
#: from_output = "/out/propolis-server-debug.sha256.txt"

set -o errexit
set -o pipefail
set -o xtrace

outdir="/out"

cargo --version
rustc --version

banner prerequisites
ptime -m ./tools/install_builder_prerequisites.sh -y

# Before we commit to building propolis-server or PHD, there are a handful of
# tests that are illumos-specific and not currently run in other test jobs. The
# whole suite doesn't take much time so we'll just run all of them here..
banner test-propolis

TEST_FEATURES="omicron-build,failure-injection"

# Set up an etherstub to use for VNICs in virtio-nic tests. We might want the
# tests to run on a real link one day to do actual networking, but we don't need
# that yet!
pfexec dladm create-etherstub prop_viona_test0
VIONA_TEST_NIC=prop_viona_test0 pfexec ptime -m \
	cargo test --verbose --features "$TEST_FEATURES"

# Build the Propolis server binary with 'dev' profile to enable assertions that
# should fire during tests.
banner build-propolis

# Compile propolis-server so that it allows development features to be used even
# though the `omicron-build` feature is enabled. This should be a relatively
# small incremental step after building and running tests with the same
# features, above.
ptime -m cargo build --verbose -p propolis-server \
	--features "$TEST_FEATURES"

# The PHD runner requires unwind-on-panic to catch certain test failures, so
# build it with the 'dev' profile which is so configured.
banner build-phd
ptime -m cargo build --verbose -p phd-runner

banner contents
tar -czvf target/debug/propolis-server-debug.tar.gz \
	-C target/debug propolis-server

tar -czvf target/debug/phd-runner.tar.gz \
	-C target/debug phd-runner \
	-C phd-tests artifacts.toml

banner copy
pfexec mkdir -p $outdir
pfexec chown "$UID" $outdir
cp .github/buildomat/phd-run-with-args.sh $outdir/phd-run-with-args.sh
mv target/debug/propolis-server-debug.tar.gz \
	$outdir/propolis-server-debug.tar.gz
mv target/debug/phd-runner.tar.gz $outdir/phd-runner.tar.gz
cd $outdir
digest -a sha256 propolis-server-debug.tar.gz > \
	propolis-server-debug.sha256.txt
digest -a sha256 phd-runner.tar.gz > phd-runner.sha256.txt
