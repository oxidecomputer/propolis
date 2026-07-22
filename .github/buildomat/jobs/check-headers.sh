#!/bin/bash
#:
#: name = "header-check"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = true
#:

# Run the various `header-check` tests across Propolis' crates.
#
# These tests are run on an illumos target for best fidelity: while the
# immediate struct and function definitions could in theory be analyzed
# anywhere, they may contain definitions that vary across target OSes. We must
# ensure, at a minimum, that FFI definitions are correct w.r.t these headers'
# interpretation on illumos. Anywhere else is just a convenience.

set -e

GATE_REF="$(./tools/check_headers gate_ref)"

# `git clone --branch` only accepts branches and tags, but GATE_REF may be an
# arbitrary ref (for example, a Gerrit change ref for an illumos-gate change
# that has not yet merged). An init-and-fetch handles any ref the remote
# advertises.
git init ./gate_src
git -C ./gate_src fetch --depth 1 \
	https://code.oxide.computer/illumos-gate "$GATE_REF"
git -C ./gate_src checkout --detach FETCH_HEAD

./tools/check_headers run ./gate_src
