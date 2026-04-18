# Define the features used for propolis testing in one place.

# PHD binaries may be built on targets that don't support nested virtualization
# - cargo build doesn't need it! - so reusing the same feature set in multiple
# places doesn't let us reuse cached build artifacts, unfortunately. This is
# just defined in one place to hedge against unexpected feature drift.
TEST_FEATUERS="omicron-build,failure-injection"
