# Propolis

Propolis is a rust-based userspace for illumos bhyve.


## Building

Given the current tight coupling of the `bhyve-api` component to the ioctl
interface presented by the bhyve kernel component, running on recent illumos
bits is required.

At a minimum, the build must include the fix for
[13275](https://www.illumos.org/issues/13275). (Present since commit
[2606939](https://github.com/illumos/illumos-gate/commit/2606939d92dd3044a9851b2930ebf533c3c03892))

While Propolis is intended to expose a REST API to drive all of its
functionality, a CLI binary using a static toml configuration is the basis for
the initial prototype.  The standard `cargo build` can be used in
`propolis-cli` to build that binary.

## Running

```
# propolis-cli <config_file>
```

Example configuration:
```toml
[main]
name = "testvm"
cpus = 4
bootrom = "/path/to/bootrom/OVMF_CODE.fd"
memory = 1024

[dev.block0]
driver = "pci-virtio-block"
disk = "/path/to/alpine-extended-3.12.0-x86_64.iso"
pci-path = "0.4.0"

[dev.net0]
driver = "pci-virtio-viona"
vnic = "vnic_name"
pci-path = "0.5.0"
```

Propolis will not destroy the VM instance on exit.  If one exists with the
specified name on start-up, it will be destroyed and and created fresh.
