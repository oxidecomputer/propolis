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

# Build the Propolis server binary with 'dev' profile to enable assertions that
# should fire during tests.
banner build-propolis

export PHD_BUILD="true"
ptime -m cargo build --verbose -p propolis-server \
	--features omicron-build,failure-injection

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
