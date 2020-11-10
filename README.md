# Propolis

Proof-of-concept bhyve userspace


## Building

Given the current tight coupling of the `bhyve-api` component to the ioctl
interface presented by the bhyve kernel component, running on recent illumos
bits is required.

At a minimum, the build must include the fix for
[13275](https://www.illumos.org/issues/13275). (Present since commit
[2606939](https://github.com/illumos/illumos-gate/commit/2606939d92dd3044a9851b2930ebf533c3c03892))

## Running

```
# propolis -r <bootrom file> <vmname>
```

Propolis will not destroy the VM instance on exit.  If one exists with the
specified name on start-up, it will be destroyed and and created fresh.
