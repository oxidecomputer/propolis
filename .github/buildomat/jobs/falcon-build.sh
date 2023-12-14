#!/bin/bash
#:
#: name = "falcon"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/work/release/*",
#: ]
#:

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

banner check
ptime -m cargo check --features falcon
ptime -m clippy --features falcon --all-targets

banner build
ptime -m cargo build --features falcon --release \
	-p propolis-server -p propolis-cli

OUTDIR=/work/release
mkdir -p $OUTDIR
cp target/release/propolis-cli $OUTDIR/propolis-cli
cp target/release/propolis-server $OUTDIR/propolis-server
