#!/bin/bash
#:
#: name = "phd-run-migrate-from-base"
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

cp /input/phd-build/out/phd-run-with-args.sh /tmp/phd-run-with-args.sh
chmod a+x /tmp/phd-run-with-args.sh
exec /tmp/phd-run-with-args.sh \
    --include-filter "phd_tests::migrate::from_base" \
    --base-propolis-branch master
