#!/bin/bash
#:
#: name = "image"
#: variety = "basic"
#: target = "helios"
#: rust_toolchain = "nightly"
#: output_rules = [
#:   "/out/*",
#: ]
#:
#: [[publish]]
#: series = "image"
#: name = "propolis-server.tar.gz"
#: from_output = "/out/propolis-server.tar.gz"
#:
#: [[publish]]
#: series = "image"
#: name = "propolis-server.sha256.txt"
#: from_output = "/out/propolis-server.sha256.txt"
#:

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

banner build
ptime -m cargo build --release --verbose -p propolis-server

banner image
ptime -m cargo run -p propolis-package

banner contents
tar tvfz out/propolis-server.tar.gz

banner copy
pfexec mkdir -p /out
pfexec chown "$UID" /out
mv out/propolis-server.tar.gz /out/propolis-server.tar.gz
cd /out
digest -a sha256 propolis-server.tar.gz > propolis-server.sha256.txt
