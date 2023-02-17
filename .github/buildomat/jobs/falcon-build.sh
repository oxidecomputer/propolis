#!/bin/bash
#:
#: name = "falcon"
#: variety = "basic"
#: target = "helios-latest"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/work/debug/*",
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

banner build
PKGS="-p propolis-server -p propolis-cli"
ptime -m cargo build --features falcon $PKGS
ptime -m cargo build --features falcon $PKGS --release

for x in debug release
do
    mkdir -p /work/$x
    cp target/$x/propolis-cli /work/$x/propolis-cli
    cp target/$x/propolis-server /work/$x/propolis-server
done
