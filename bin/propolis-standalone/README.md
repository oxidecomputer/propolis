# Propolis Standalone

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
`screen`) would also work.

## Quickstart to Alpine

In the aforementioned config files, there are three major components
that need to be supplied: The guest firmware (bootrom) image, the ISO, and the
VNIC.

Since this is a configuration file, you can supply whatever you'd like, but here
are some options to get up-and-running quickly:

### Guest bootrom

The current recommended and tested guest bootrom is available
[here](https://buildomat.eng.oxide.computer/public/file/oxidecomputer/edk2/image_debug/6d92acf0a22718dd4175d7c64dbcf7aaec3740bd/OVMF_CODE.fd).

Other UEFI firmware images built from the [Open Virtual Machine Firmware
project](https://github.com/tianocore/tianocore.github.io/wiki/OVMF) may also
work, but these aren't regularly tested and your mileage may vary.

### ISO

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

### VNIC

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

### Running a VM

After you've got the bootrom, an ISO, a VNIC, and a configuration file that
points to them, you're ready to create and run your VM. To do so, make sure
you've done the following:
- [build propolis](#Building)
- run the [propolis-server](#propolis-server)
- create your VM, run it, and hop on the serial console using [propolis-cli](#propolis-cli)
- login to the VM as root (no password)
- optionally, run `setup-alpine` to configure the VM (including setting a root
  password)

## Using Crucible storage

`propolis-standalone` supports defining crucible-backed storage devices in the
TOML config. It is somewhat inconvenient to do this without scripting, because
`generation` must monotonically increase with each successive connection to the
Downstairs datastore. So if you use this, you need to somehow monotonically bump
up that number in the TOML file before re-launching the VM, unless you're also
creating a new Downstairs region from scratch.

All the crucible configuration options are crucible-specific, so future changes
to crucible may result in changes to the config options here as well. Consult
the [oxidecomputer/crucible](https://github.com/oxidecomputer/crucible) codebase
if you need low level details on what certain options actually do.

Here's an example config. Read the comments for parameter-specific details:

```toml
[block_dev.some_datastore]
type = "crucible"

# === REQUIRED OPTIONS ===
# these MUST match the region configuration downstairs
block_size = 512
blocks_per_extent = 262144
extent_count = 32

# Array of the SocketAddrs of the Downstairs instances. There must be three
# of these, or propolis-standalone will panic.
targets = [
  "127.0.0.1:3810",
  "127.0.0.1:3820",
  "127.0.0.1:3830",
]

# Generation number used when connecting to Downstairs. This must
# monotonically increase with each successive connection to the Downstairs,
# which means that you need to bump this number every time you restart
# your VM. Kind of annoying, maybe we can get a better way to pass it in.
# Anyway, if you don't want to read-modify-write this value, a hack you
# could do is set this to the current number of seconds since the epoch.
# This'll always work, except for if the system time goes backwards, which
# it can definitely do! So, you know. Be careful.
generation = 1
# === END REQUIRED OPTIONS ===


# === OPTIONAL OPTIONS ===
# This should be a UUID. It can be anything, really. When unset, defaults
# to a random UUIDv4
# upstairs_id = "e4396bd0-ede1-48d7-ac14-3d2094dfba5b"

# When true, some random amount of IO requests will synthetically "fail".
# This is useful when testing IO behavior under Bad Conditions.
# Defaults to false.
# lossy = false

# the Upstairs (propolis-side) component of crucible currently regularly
# dispatches flushes to act as IO barriers. By default this happens once every 5
# seconds, but you can adjust it with this option.
# flush_timeout = <number>

# Base64'd encryption key used to encrypt data at rest. Keys are 256 bits.
# Note that the region must have already been created with encryption
# enabled for this to work. That may change later though.
# encryption_key = ""

# These three values are pem files for TLS encryption of data between
# propolis and the downstairs.
# cert_pem = ""
# key_pem = ""
# root_cert_pem = ""

# Specifies the SocketAddr of the Upstairs crucible control interface. When
# ommitted, the control interface won't be started. The control interface is an
# HTTP server that exposes commands to take snapshots, simulate faults, and
# retrieve runtime debug information.
# control_addr = ""

# When true, the device will be read-only. Defaults to false
# read_only = false
# === END OPTIONAL OPTIONS ===
```
