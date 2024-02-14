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

set -o errexit
set -o pipefail
set -o xtrace

indir="/input"
indir_suffix="phd-build/out/*.tar.gz"
phddir="$PWD/phd-test"

# Put artifacts on the runner's SSDs (the /work ramdisk is small by design, too
# small for images of any appreciable size).
pfexec zpool create -f phd-artifacts c1t1d0 c2t1d0
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

banner 'Tests'

runner="$phddir/phd-runner"
artifacts="$phddir/artifacts.toml"
propolis="$phddir/propolis-server"

ls $runner
ls $artifacts
ls $propolis

# Disable errexit so that we still upload logs on failure
set +e

(RUST_BACKTRACE=1 RUST_LOG="info,phd=debug" ptime -m pfexec $runner \
	--emit-bunyan \
	run \
	--propolis-server-cmd $propolis \
	--base-propolis-branch master \
	--crucible-downstairs-commit auto \
	--artifact-toml-path $artifacts \
	--tmp-directory $tmpdir \
	--artifact-directory $artifactdir | \
	tee /tmp/phd-runner.log)
failcount=$?
set -e

tar -czvf /tmp/phd-tmp-files.tar.gz \
	-C /tmp/propolis-phd /tmp/propolis-phd/*.log \
	-C /tmp/propolis-phd /tmp/propolis-phd/*.toml

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
