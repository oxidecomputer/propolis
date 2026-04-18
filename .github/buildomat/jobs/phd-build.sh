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
#:
#: [[publish]]
#: series = "propolis_tests"
#: name = "propolis-tests.tar.gz"
#: from_output = "/out/propolis-tests-debug.tar.gz"

set -o errexit
set -o pipefail
set -o xtrace

outdir="/out"

cargo --version
rustc --version

banner prerequisites
ptime -m ./tools/install_builder_prerequisites.sh -y

# Build the Propolis server binary with 'dev' profile to enable assertions that
# should fire during tests.
banner build-propolis

# We'll do a few cargo builds, keeping features the same means we reuse build
# artifacts from crates these configure.
TEST_FEATURES="omicron-build,failure-injection"

# Compile propolis-server so that it allows development features to be used even
# though the `omicron-build` feature is enabled. This should be a relatively
# small incremental step after building and running tests with the same
# features, above.
export PHD_BUILD="true"
ptime -m cargo build --verbose -p propolis-server \
	--features "$TEST_FEATURES"

# Build Propolis test binaries, but they won't run here. The path to get here
# is unfortunate:
# * we don't have `git` on a Gimlet target.
# * or `pkg`.
# * `uname -m` is "oxide", which confuses rustup too.
#
# Setting this up by hand is "possible", but it's much easier to just build the
# test binaries here and squirrel them off to *run* on a Gimlet. So do that.
ptime -m cargo build --tests --verbose --features "$TEST_FEATURES"

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

tar -czvf target/debug/propolis-tests-debug.tar.gz \
	$(find target/debug/deps/ -perm -111 -type f -name 'propolis-*')

banner copy
pfexec mkdir -p $outdir
pfexec chown "$UID" $outdir
cp .github/buildomat/phd-run-with-args.sh $outdir/phd-run-with-args.sh
mv target/debug/propolis-server-debug.tar.gz \
	$outdir/propolis-server-debug.tar.gz
mv target/debug/phd-runner.tar.gz $outdir/phd-runner.tar.gz
mv target/debug/propolis-tests-debug.tar.gz $outdir/propolis-tests-debug.tar.gz
cd $outdir
digest -a sha256 propolis-server-debug.tar.gz > \
	propolis-server-debug.sha256.txt
digest -a sha256 phd-runner.tar.gz > phd-runner.sha256.txt
