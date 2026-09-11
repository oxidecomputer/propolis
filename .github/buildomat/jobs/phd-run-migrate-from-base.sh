#!/bin/bash
#:
#: name = "phd-run-migrate-from-base"
#: variety = "basic"
#: target = "lab-2.0-gimlet"
#: output_rules = [
#:	"/tmp/phd-runner.log",
#:	"/tmp/phd-tmp-files.tar.gz",
#: ]
#: skip_clone = true
#:
#: [dependencies.phd-build]
#: job = "phd-build"
#:

# This job runs the PHD migrate-from-base tests, which test upgrading from the
# current mainline propolis-server to the propolis-server version under test.
#
# PHD always uses the propolis-client from the commit under test, so these tests
# will fail if there is a breaking change to the Propolis API. They'll also fail
# if there's a breaking change to the migration protocol. These changes may be
# expected, in which case this run will fail. However, the "regular" phd-run
# job should always be green before merging new PRs.
#
# This job will be removed once API breaking changes are no longer allowed.

cp /input/phd-build/out/phd-run-with-args.sh /tmp/phd-run-with-args.sh
chmod a+x /tmp/phd-run-with-args.sh
exec /tmp/phd-run-with-args.sh \
    --include-filter "phd_tests::migrate::from_base" \
    --base-propolis-branch master
