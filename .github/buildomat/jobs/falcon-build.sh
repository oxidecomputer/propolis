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
#: [[publish]]
#: series = "falcon"
#: name = "propolis-server"
#: from_output = "/work/release/propolis-server"
#: 
#: [[publish]]
#: series = "falcon"
#: name = "propolis-server.sha256.txt"
#: from_output = "/work/release/propolis-server.sha256.txt"
#:
#: [[publish]]
#: series = "falcon"
#: name = "propolis-cli"
#: from_output = "/work/release/propolis-cli"
#:
#: [[publish]]
#: series = "falcon"
#: name = "propolis-cli.sha256.txt"
#: from_output = "/work/release/propolis-cli.sha256.txt"

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

banner check
ptime -m cargo check --features falcon
ptime -m cargo clippy --features falcon --all-targets

banner build
ptime -m cargo build --features falcon --release \
	-p propolis-server -p propolis-cli

OUTDIR=/work/release
mkdir -p $OUTDIR
cp target/release/propolis-cli $OUTDIR/propolis-cli
cp target/release/propolis-server $OUTDIR/propolis-server

cd $OUTDIR
digest -a sha256 propolis-cli > propolis-cli.sha256.txt
digest -a sha256 propolis-server > propolis-server.sha256.txt
