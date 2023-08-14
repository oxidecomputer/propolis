# Run propolis-standalone with crucible disks

This document serves as an overview for running propolis-standalone using
crucible disks.

## Background: Why standalone?

In the product, the userspace VMM component for instances is
[propolis-server](https://github.com/oxidecomputer/propolis/tree/master/bin/propolis-server),
which exposes API endpoints for the rest of the control plane to manage
instances. One can run propolis-server in isolation, such as for development,
and interact with its endpoints using a
client such as the [propolis
CLI](https://github.com/oxidecomputer/propolis/tree/master/bin/propolis-cli).


The
[standalone](https://github.com/oxidecomputer/propolis/tree/master/bin/propolis-standalone#propolis-standalone)
version of propolis is useful development tool to a VM up and running quickly
without having to hit any API endpoints. It takes an input a single TOML file,
sets up a unix domain socket that is connected to a VM's uart, and starts the
guests when the user connects to the socket. It also cleans up the VM gracefully
when the user sends CTRL+C to the running `propolis-standalone` program.

Beyond these differences, there are some differences in the state machine
related to the instance lifecycle as well as the emulation.

TODO: flesh out more of these differences, and maybe capture them in a
higher-level README.

## Requirements

TODO: document the following

- how to build each of propolis and crucible
- assuming we are running on bench gimlets, which files to copy


## Instructions

