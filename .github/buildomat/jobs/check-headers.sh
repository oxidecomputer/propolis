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

# TODO: `--branch` is overly restrictive, but it's what we've got. In git 2.49
# the --revision flag was added to `git-clone`, and can clone an arbitrary
# revision, which is more appropriate here. We might be tracking an arbitrary
# commit with some changes in illumos-gate that isn't yet merged, after all.
git clone --depth 1 --branch "$GATE_REF" \
	https://code.oxide.computer/illumos-gate ./gate_src

./tools/check_headers run ./gate_src
