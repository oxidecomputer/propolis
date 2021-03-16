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

Propolis will create a unix domain socket, available at "./ttya",
which acts as a serial port. One such tool for accessing this serial port is
[sercons](https://github.com/jclulow/vmware-sercons), though others (such as
"screen") would also work.

### Quickstart to Alpine

In the aforementioned config file, there are three major components
that need to be supplied: The OVMF file, the ISO, and the VNIC.

Since this is a configuration file, you can supply whatever you'd like, but here
are some options to get up-and-running quickly:

#### OVMF

Using a bootrom from Linux works here - you can either build
your own [OVMF](https://wiki.ubuntu.com/UEFI/OVMF), or you
can use a pre-built.

```bash
$ sudo apt-get install ovmf && dpkg -L ovmf | grep OVMF_CODE.fd
```

#### ISO

Although there are many options for ISOs, an easy option that
should work is the [Alpine Linux distribution](https://alpinelinux.org/downloads/).

These distributions are lightweight, and they have varients
custom-built for virtual machines.

The "extendend" variant contains more useful tools, but will
require a modification of the kernel arguments when booting
to see the console on the serial port. From Grub, this can be
accomplished by pressing "e" (to edit), adding "console=ttyS0"
to the line starting with "/boot/vmlinuz-lts", and pressing
"Control + x" to boot with these parameters.

#### VNIC

To see your current network interfaces, you can use the following:

```bash
$ dladm show-link
```

To create a vnic, you can use one of your physical devices
(like "e1000g0", if you have an ethernet connection) as a link
for a VNIC. This can be done as follows:

```bash
NIC_NAME="vnic_prop0"
NIC_MAC="02:08:20:ac:e9:16"
NIC_LINK="e1000g0"

if ! dladm show-vnic $NIC_NAME 2> /dev/null; then
  dladm create-vnic -t -l $NIC_LINK -m $NIC_MAC $NIC_NAME
fi
```

## License

Unless otherwise noted, all components are licensed under the [Mozilla Public
License Version 2.0](LICENSE).
