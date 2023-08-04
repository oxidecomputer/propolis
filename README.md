# Propolis

Propolis is a rust-based userspace for illumos bhyve.

## Prerequisites

Given the current tight coupling of the `bhyve-api` component to the ioctl
interface presented by the bhyve kernel component, running on recent illumos
bits is required.

Propolis works best (and its CI tests run) on AMD hosts, but it can also be used
to run VMs on Intel hosts. Live migration is primarily supported on AMD hosts
but may work on Intel hosts as well.

## Building

To build, run:

```bash
$ cargo build
```

## propolis crate

The main propolis crate is structured as a library providing the building
blocks to create bhyve backed VM instances. It also provides a number of
emulated devices that can be exposed to guests (e.g. serial port, virtio
devices, NVMe).

## propolis-server

Propolis is mostly intended to be used via a REST API to drive all of its
functionality. The standard `cargo build` will produce a `propolis-server`
binary you can run:

### Running

```
# propolis-server run <config_file> <ip:port>
```

Note that the server must run as root. One way to ensure propolis-server has
sufficient privileges is by using `pfexec(1)`, as such:

```
# pfexec propolis-server run <config_file> <ip:port>
```

### Example Server Configuration

**Note**: the goal is to move the device config from the toml
to instead be configured via REST API calls.

```toml
bootrom = "/path/to/bootrom/OVMF_CODE.fd"

[block_dev.alpine_iso]
type = "file"
path = "/path/to/alpine-extended-3.12.0-x86_64.iso"

[dev.block0]
driver = "pci-virtio-block"
block_dev = "alpine_iso"
pci-path = "0.4.0"

[dev.net0]
driver = "pci-virtio-viona"
vnic = "vnic_name"
pci-path = "0.5.0"
```

## propolis-cli

Once you've got `propolis-server` running you can interact with it via the REST
API with any of the usual suspects (e.g. cURL, wget). Alternatively, there's a
`propolis-cli` binary to make things a bit easier:

### Running

The following CLI commands will create a VM, start the VM, and then attach to
its serial console:

```
# propolis-cli -s <propolis ip> -p <propolis port> new <VM name>
# propolis-cli -s <propolis ip> -p <propolis port> state <VM name> run
# propolis-cli -s <propolis ip> -p <propolis port> serial <VM name>
```

## propolis-standalone

Server frontend aside, we also provide a standalone binary for quick
prototyping, `propolis-standalone`. It uses a static toml configuration:

## Running

```
# propolis-standalone <config_file>
```

Example configuration:
```toml
[main]
name = "testvm"
cpus = 4
bootrom = "/path/to/bootrom/OVMF_CODE.fd"
memory = 1024

[block_dev.alpine_iso]
type = "file"
path = "/path/to/alpine-extended-3.12.0-x86_64.iso"

[dev.block0]
driver = "pci-virtio-block"
block_dev = "alpine_iso"
pci-path = "0.4.0"

[dev.net0]
driver = "pci-virtio-viona"
vnic = "vnic_name"
pci-path = "0.5.0"
```

Propolis will not destroy the VM instance on exit.  If one exists with the
specified name on start-up, it will be destroyed and created fresh.

Propolis will create a unix domain socket, available at "./ttya",
which acts as a serial port. One such tool for accessing this serial port is
[sercons](https://github.com/jclulow/vmware-sercons), though others (such as
"screen") would also work.

### Quickstart to Alpine

In the aforementioned config files, there are three major components
that need to be supplied: The guest firmware (bootrom) image, the ISO, and the
VNIC.

Since this is a configuration file, you can supply whatever you'd like, but here
are some options to get up-and-running quickly:

#### Guest bootrom

The current recommended and tested guest bootrom is available
[here](https://buildomat.eng.oxide.computer/public/file/oxidecomputer/edk2/image_debug/6d92acf0a22718dd4175d7c64dbcf7aaec3740bd/OVMF_CODE.fd).

Other UEFI firmware images built from the [Open Virtual Machine Firmware
project](https://github.com/tianocore/tianocore.github.io/wiki/OVMF) may also
work, but these aren't regularly tested and your mileage may vary.

#### ISO

Although there are many options for ISOs, an easy option that
should work is the [Alpine Linux distribution](https://alpinelinux.org/downloads/).

These distributions are lightweight, and they have variants
custom-built for virtual machines.

A straightforward option to start with is the "virtual" `x86_64` image.

The "extended" variant contains more useful tools, but will require a
modification of the kernel arguments when booting to see the console on the
serial port.  From Grub, this can be accomplished by pressing "e" (to edit),
adding "console=ttyS0" to the line starting with "/boot/vmlinuz-lts", and
pressing "Control + x" to boot with these parameters.

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

#### Running a VM

After you've got the bootrom, an ISO, a VNIC, and a configuration file that
points to them, you're ready to create and run your VM. To do so, make sure
you've done the following:
- [build propolis](#Building)
- run the [propolis-server](#propolis-server)
- create your VM, run it, and hop on the serial console using [propolis-cli](#propolis-cli)
- login to the VM as root (no password)
- optionally, run `setup-alpine` to configure the VM (including setting a root
  password)

## License

Unless otherwise noted, all components are licensed under the [Mozilla Public
License Version 2.0](LICENSE).
