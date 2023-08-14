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
- propolis-standalone-config: Type definitions for `propolis-standalone` config file
- propolis-types: Publically exposed (via `propolis-server`) types, intergral
  to the `propolis` library
- viona-api: API (ioctls & structs) for the illumos viona driver

## License

Unless otherwise noted, all components are licensed under the [Mozilla Public
License Version 2.0](LICENSE).
