#!/bin/bash
#:
#: name = "phd-build"
#: variety = "basic"
#: target = "helios"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/out/*",
#: ]

set -o errexit
set -o pipefail
set -o xtrace

outdir="/out"

cargo --version
rustc --version

# Build the Propolis server binary in debug mode to enable assertions
# that should fire during tests.
banner build-propolis
ptime -m cargo build --verbose -p propolis-server

# Build the PHD runner with the phd profile to enable unwind on panic,
# which the framework requires to catch certain test failures.
banner build-phd
ptime -m cargo build --verbose -p phd-runner --profile=phd

banner contents
tar -czvf target/debug/propolis-server-debug.tar.gz \
	-C target/debug propolis-server

tar -czvf target/phd/phd-runner.tar.gz \
	-C target/phd phd-runner \
	-C phd-tests artifacts.toml

banner copy
pfexec mkdir -p $outdir
pfexec chown "$UID" $outdir
mv target/debug/propolis-server-debug.tar.gz \
	$outdir/propolis-server-debug.tar.gz
mv target/phd/phd-runner.tar.gz $outdir/phd-runner.tar.gz
cd $outdir
digest -a sha256 propolis-server-debug.tar.gz > \
	propolis-server-debug.sha256.txt
digest -a sha256 phd-runner.tar.gz > phd-runner.sha256.txt
