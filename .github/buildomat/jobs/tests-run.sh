#!/bin/bash
#:
#: name = "tests-run"
#: variety = "basic"
#: target = "lab"
#: output_rules = [
#:	"/tmp/*.log",
#: ]
#: skip_clone = true
#:
#: [dependencies.build]
#: job = "tests-build"
#:

set -o errexit
set -o pipefail
set -o xtrace

banner 'Inputs'
find /input -ls

testdir="$PWD/tests"
rm -rf "$testdir"
mkdir "$testdir"

for p in /input/build/work/tests/*.gz; do
	f="$testdir/$(basename "$p")"
	f="${f%.gz}"
	rm -f "$f"
	gunzip < "$p" > "$f"
	chmod +x "$f"
done

fail=no

banner 'Tests'
for f in "$testdir"/*; do
	#
	# Tests generally need to run as root in order to have access to the
	# hypervisor device.
	#
	if ! ptime -m pfexec "$f" --show-output --test-threads 1; then
		echo
		echo "TEST $f FAILED"
		echo
		fail=yes
	else
		echo
		echo "TEST $f PASSED"
		echo
	fi
done

if [[ $fail == yes ]]; then
	echo "SOME TESTS FAILED"
	exit 1
else
	echo "ALL TESTS PASSED"
	exit 0
fi
