# Propolis

Propolis VMM userspace for use with illumos bhyve.

## Prerequisites

Given the current tight coupling of the `bhyve-api` component to the ioctl
interface presented by the bhyve kernel component, running on recent illumos
bits is required.

Propolis works best (and its CI tests run) on AMD hosts, but it can also be used
to run VMs on Intel hosts. Live migration is primarily supported on AMD hosts
but may work on Intel hosts as well.

## Components

Programs:
- [propolis-server](bin/propolis-server): Run a Propolis VM instance, operated
  via REST API calls (typically by
  [omicron](https://github.com/oxidecomputer/omicron))
- [propolis-cli](bin/propolis-cli): CLI wrapper interface for `propolis-server`
  API calls
- [propolis-standalone](bin/propolis-standalone): Simple standalone program to
  run a Propolis VM instance, operated via a local config file

Libraries:
- [propolis-client](lib/propolis-client): Rust crate for `propolis-server` API
- [propolis](lib/propolis): Represents the bulk of the emulation logic required
  to implement a userspace VMM.  Both `propolis-server` and
  `propolis-standalone` are built around this.

## Internal Crates

These are not meant as committed public interfaces, but rather internal
implementation details, consumed by Propolis components.

- bhyve-api: API (ioctls & structs) for the illumos bhyve kernel VMM
- dladm: Some thin wrappers around `dladm` queries
- propolis-server-config: Type definitions for `propolis-server` config file
- propolis-types: Publically exposed (via `propolis-server`) types, intergral
  to the `propolis` library
- viona-api: API (ioctls & structs) for the illumos viona driver

## xtasks

Propolis uses the `cargo xtask` pattern in order to conveniently expose certain
tasks to developers.

- `clippy`: Run suite of clippy checks.  This performs more than a simple
  `cargo clippy`, since there are several combinations of feature flags which
  must be checked.
- `fmt`: Check style according to `rustfmt`
- `license`:  Check (crudely) that files bear appropriate license headers
- `phd`: Run the PHD test suite
- `style`: Perform miscellaneous style checks
- `prepush`: Preform pre-push checks (`clippy`, `fmt`, `license`, `style`) in a
  manner which resembles (but does not exactly match) how they are run in CI.
  Running tests (unit, integration, or phd) are not included and are left to
  the user.

It is recommended that developers run the `prepush` test before pushing a
branch which will be subsequently checked by CI.  Doing so currently requires
an x86\_64 UNIX/Linux machine.

## License

Unless otherwise noted, all components are licensed under the [Mozilla Public
License Version 2.0](LICENSE).
