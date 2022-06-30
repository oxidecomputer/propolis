#!/bin/bash
#:
#: name = "tests-build"
#: variety = "basic"
#: target = "helios"
#: rust_toolchain = "stable"
#: output_rules = [
#:	"/work/bins/*.gz",
#:	"/work/tests/*.gz",
#: ]
#:

set -o errexit
set -o pipefail
set -o xtrace

/opt/ooce/bin/jq --version
cargo --version
rustc --version

#banner 'BuildBins'
#ptime -m cargo build --verbose --bins \
#    --message-format json-render-diagnostics >/tmp/output.build.json

banner 'BuildTests'
ptime -m cargo build --verbose --tests \
    --message-format json-render-diagnostics >/tmp/output.tests.json

function artifacts_from {
	/opt/ooce/bin/jq -r -s "
		map(
			select(.reason == \"compiler-artifact\")
			| select(
				.target.kind
				| map_values(. == \"$2\")
				| any
			)
			| .filenames
		)
		| flatten
		| .[]" "$1"
}

#banner 'SaveBins'
#mkdir -p /work/bins
#artifacts_from /tmp/output.build.json bin | while read a; do
#	ptime -m gzip < "$a" > "/work/bins/$(basename "$a").gz"
#done

banner 'SaveTests'
mkdir -p /work/tests
artifacts_from /tmp/output.tests.json test | while read a; do
	ptime -m gzip < "$a" > "/work/tests/$(basename "$a").gz"
done

banner 'Done'
ls -lh /work/*/*
