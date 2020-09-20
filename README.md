# Propolis

Proof-of-concept bhyve userspace


## Building

If developing on Linux, set `CARGO_BUILD_TARGET=x86_64-unknown-illumos` in the
environment to get proper function signature for `ioctl()` and friends.
