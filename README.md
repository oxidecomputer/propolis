# Propolis

Proof-of-concept bhyve userspace


## Building

Given the current tight coupling of the `bhyve-api` component to the ioctl
interface presented by the bhyve kernel component, running on recent illumos
bits is required.

At a minimum, the build must include the fix for
[12989](https://www.illumos.org/issues/12989). (Present since commit
[e0c0d44](https://github.com/illumos/illumos-gate/commit/e0c0d44e917080841514d0dd031a696c74e8c435))

## Running

```
# propolis -r <bootrom file> <vmname>
```

Propolis will not destroy the VM instance on exit.  If one exists with the
specified name on start-up, it will be destroyed and and created fresh.
