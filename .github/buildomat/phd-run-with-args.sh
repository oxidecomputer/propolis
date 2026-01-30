#!/bin/bash

# Unpacks and executes the PHD runner in the Buildomat environment, passing
# through to the runner any arguments that were passed to this script.

set -o errexit
set -o pipefail
set -o xtrace

indir="/input"
indir_suffix="phd-build/out/*.tar.gz"
phddir="$PWD/phd-test"

# Put artifacts on the runner's SSDs (the /work ramdisk is small by design, too
# small for images of any appreciable size).

# Find usable disks to make a zpool on. Note that this only works on Oxide
# compute sled runners such as `lab-2.0-gimlet`.
disks=( $(pilot local disk list -H -o type,disk | grep -v 'M.2' | cut -f 2) )
pfexec zpool create -f phd-artifacts ${disks[@]}
artifactdir="/phd-artifacts"

banner 'Inputs'
find $indir -ls

rm -rf "$phddir"
mkdir "$phddir"

for p in $indir/$indir_suffix; do
	tar xzvf $p -C $phddir
	for f in $(tar tf "$p"); do
		chmod +x "$phddir/$f"
	done
done

ls $phddir

banner 'Setup'
tmpdir="/tmp/propolis-phd"
if [ ! -d "$tmpdir" ]; then
	mkdir $tmpdir
fi

# We'll be using the reservoir, so set it to something higher than the default
# 0MiB. Most tests would only need 512MiB (the default PHD guest VM memory
# size), some tests want to run multiple VMs concurrently (particularly around
# migration). 4096MiB is an arbitrary number intended to support the above and
# that we might want to run those tests concurrently at some point.
#
# Currently the lab host these tests will run on is well-known and has much
# more memory than this. Hopefully we won't have Propolis CI running on a
# machine with ~4GiB of memory, but this number could be tuned down if the need
# arises.
pfexec /usr/lib/rsrvrctl -s 4096

banner 'Tests'

runner="$phddir/phd-runner"
artifacts="$phddir/artifacts.toml"
propolis="$phddir/propolis-server"

ls $runner
ls $artifacts
ls $propolis

args=(
    $runner '--emit-bunyan' 'run'
    '--propolis-server-cmd' $propolis
    '--crucible-downstairs-commit' 'auto'
    '--artifact-toml-path' $artifacts
    '--tmp-directory' $tmpdir
    '--artifact-directory' $artifactdir
    $@
)

# Disable errexit so that we still upload logs on failure
set +e
(RUST_BACKTRACE=1 RUST_LOG="info,phd=debug" ptime -m pfexec "${args[@]}" | \
    tee /tmp/phd-runner.log)
failcount=$?
set -e

tar -czvf /tmp/phd-tmp-files.tar.gz \
	-C /tmp/propolis-phd /tmp/propolis-phd/*.log

exitcode=0
if [ $failcount -eq 0 ]; then
	echo
	echo "ALL TESTS PASSED"
	echo
else
	echo
	echo "SOME TESTS FAILED"
	echo
	exitcode=1
fi

if find /dev/vmm -mindepth 1 -maxdepth 1 | read ; then
	echo "VMM handles leaked:"
	find /dev/vmm -type c -exec basename {} \;
	exitcode=2
fi

exit $exitcode
