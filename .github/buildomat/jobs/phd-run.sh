#!/bin/bash
#:
#: name = "phd-run"
#: variety = "basic"
#: target = "lab-2.0-opte"
#: output_rules = [
#:	"/tmp/phd-runner.log",
#:	"/tmp/phd-tmp-files.tar.gz",
#: ]
#: skip_clone = true
#:
#: [dependencies.phd-build]
#: job = "phd-build"
#:

# This job runs all the PHD test cases that don't involve upgrading from an
# earlier version of Propolis.
#
# These tests should always pass even in the presence of breaking changes to the
# Propolis API or live migration protocol.

cp /input/phd-build/out/phd-run-with-args.sh /tmp/phd-run-with-args.sh
chmod a+x /tmp/phd-run-with-args.sh
export PHD_DEFAULT_ARTIFACT="windows_server_2022"
export PHD_DEFAULT_ARTIFACT_FILENAME="windows-server-2022-genericcloud-amd64-phd.raw"
exec /tmp/phd-run-with-args.sh --default-guest-memory-mib 1024 \
    --exclude-filter "phd_tests::migrate::from_base"
