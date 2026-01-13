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

git clone --depth 1 --revision "$GATE_REF" \
	https://github.com/oxidecomputer/illumos-gate ./gate_src

./tools/check_headers run ./gate_src
