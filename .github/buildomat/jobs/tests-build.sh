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

# For each entry in the supplied Cargo build log:
# - Select all those entries with a reason of "compiler-artifact"
# - Filter those entries as follows:
#   - Look at the values in the target kind map
#   - Map each value to true/false by comparing it to $2
#   - Keep the entry only if at least one value matched the desired kind
# - Get an array of the entries that passed the filter, preserving only their
#   filename fields
# - Flatten the array into an array of filenames
# - Print the array entries unadorned, one to a line
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
