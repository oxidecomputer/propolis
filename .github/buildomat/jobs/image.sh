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

banner prerequisites
ptime -m ./tools/install_builder_prerequisites.sh -y

banner build

# We collect some build envrionment information with vergen to augment
# development builds, but this would otherwise put extra build-env info into
# the production (and test) binaries built here. While we don't test or rely on
# deterministic rebuilds of propolis-server, set Vergen up to use idempotent
# environment variables so we're not needlessly introducing build-time
# variance.
#
# "idempotent" still lets us collect the current relevant git info, so CI
# builds of propolis-server will still be able to self-report what commit they
# came from. See
# https://docs.rs/vergen/10.0.3/vergen/struct.Emitter.html#method.idempotent
# for more.
export VERGEN_IDEMPOTENT="true"

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
