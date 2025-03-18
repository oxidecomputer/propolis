#!/bin/bash
#:
#: name = "phd-run-windows"
#: variety = "basic"
#: target = "lab-2.0-opte"
#: output_rules = [
#:	"/tmp/phd-runner.log",
#:	"/tmp/phd-tmp-files.tar.gz",
#: ]
#: skip_clone = true
#: enable = false
#:
#: [dependencies.phd-build]
#: job = "phd-build"
#:

# This job runs all the PHD test cases that don't involve upgrading from an
# earlier version of Propolis under a Windows guest.
#
# These tests *should* pass. As of writing, most do, but not all. The test
# failures we've looked at are benign mismatches between a test's expected
# serial console output and otherwise-correct guest behavior.
#
# Between the expected overall failure of the job and that Windows guests take
# longer to start and stop (and so result in a more-than-double overall CI
# exeuction time), this is disabled by default and only run as needed.

cp /input/phd-build/out/phd-run-with-args.sh /tmp/phd-run-with-args.sh
chmod a+x /tmp/phd-run-with-args.sh

export PHD_DEFAULT_ARTIFACT="windows_server_2022"
export PHD_DEFAULT_ARTIFACT_FILENAME="windows-server-2022-genericcloud-amd64-phd.raw"
# Windows guests seem to just hang when booted with 512mb of memory. 1GiB is
# enough to proceed.
exec /tmp/phd-run-with-args.sh --default-guest-memory-mib 1024 \
    --exclude-filter "phd_tests::migrate::from_base"
