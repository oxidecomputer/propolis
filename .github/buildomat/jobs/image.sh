#!/bin/bash
#:
#: name = "image"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = "stable"
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

# Enable the "omicron-build" feature to indicate this is an artifact destined
# for production use on an appropriately configured Oxide machine
#
# The 'release' profile is configured for abort-on-panic, so we get an
# immediate coredump rather than unwinding in the case of an error.
ptime -m cargo build --release --verbose -p propolis-server --features omicron-build

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
