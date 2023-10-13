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
# File path to store qemu debug port output (optional)
# qemu_debug_file = "/path/to/output"

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
- build `propolis-standalone`
- start `propolis-standalone`, passing it a valid config
- it will wait to start the VM until you connect to the serial console socket
  (with something like [sercons](https://github.com/jclulow/vmware-sercons))
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
## Configuring `cpuid`

Rather than using the built-in `cpuid` data masking offered by the bhyve kernel
VMM, propolis-standalone can load a set of leaf data to be used by the instance.
An example of such configuration data is as follows:

```toml
[main]
# ... other main config bits
cpuid_profile = "NAME"

[cpuid.NAME]
vendor = "amd"
"0" = [0x10, 0x68747541, 0x444d4163, 0x69746e65]
"1" = [0x830f10, 0x10800, 0xf6d83203, 0x178bfbff]
"5" = [0x0, 0x0, 0x0, 0x0]
"6" = [0x4, 0x0, 0x0, 0x0]
"7" = [0x0, 0x0, 0x0, 0x0]
"7-0" = [0x0, 0x201401a9, 0x0, 0x0]
"d" = [0x0, 0x0, 0x0, 0x0]
"d-0" = [0x7, 0x340, 0x340, 0x0]
"d-1" = [0x1, 0x0, 0x0, 0x0]
"d-2" = [0x100, 0x240, 0x0, 0x0]
"80000000" = [0x80000020, 0x68747541, 0x444d4163, 0x69746e65]
"80000001" = [0x830f10, 0x40000000, 0x444031fb, 0x25d3fbff]
"80000002" = [0x20444d41, 0x43595045, 0x38323720, 0x36312032]
"80000003" = [0x726f432d, 0x72502065, 0x7365636f, 0x20726f73]
"80000004" = [0x20202020, 0x20202020, 0x20202020, 0x202020]
"80000005" = [0xff40ff40, 0xff40ff40, 0x20080140, 0x20080140]
"80000006" = [0x48006400, 0x68006400, 0x2006140, 0x2009140]
"80000007" = [0x0, 0x0, 0x0, 0x100]
"80000008" = [0x3030, 0x7, 0x0, 0x10000]
"8000000a" = [0x1, 0x8000, 0x0, 0x13bcff]
"80000019" = [0xf040f040, 0x0, 0x0, 0x0]
"8000001a" = [0x6, 0x0, 0x0, 0x0]
"8000001b" = [0x3ff, 0x0, 0x0, 0x0]
"8000001d" = [0x0, 0x0, 0x0, 0x0]
"8000001d-0" = [0x121, 0x1c0003f, 0x3f, 0x0]
"8000001d-1" = [0x122, 0x1c0003f, 0x3f, 0x0]
"8000001d-2" = [0x143, 0x1c0003f, 0x3ff, 0x2]
"8000001d-3" = [0x163, 0x3c0003f, 0x3fff, 0x1]
"8000001f" = [0x1000f, 0x16f, 0x1fd, 0x1]
```

If `cpuid_profile` is specified under the `main` section, a corresponding
`cpuid` section with a matching name is expected to be defined elsewhere in the
file.  The `vendor` field under that section controls fallback behavior when a
vCPU queries a non-existent leaf, and other CPU-specific behavior.  After that,
the leafs and their register data are listed.  Leafs which require an `ecx`
match (with `eax` as the function, and `ecx` as the index) are specified with a
hyphen separating the function and index.  Leafs without an index (just a single
hex number) will match only against `eax`, and at a lower priority than the
function/index leafs which match `eax` and `ecx`.  The data for leafs is
expected to be a 4-item array of 32-bit integers corresponding to `eax`, `ebx`,
`ecx`, and `edx`, in that order.

Certain fields in `cpuid` data depend on aspects specific to the host (such as
vCPU count) or the vCPU they are associated with (such as APIC ID).  Propolis
will "specialize" the data provided in the `cpuid` profile with logic appropriate
for the specific leafs involved.

## Configuring Cloud-Init

Propolis is able to assemble a disk image formatted in the
[NoCloud](https://cloudinit.readthedocs.io/en/latest/reference/datasources/nocloud.html)
fashion to be consumed by `cloud-init` inside the guest.  An example of such configuration is as follows:
```toml
# ... other configuration bits

# Define a disk device to bear the cloud-init data
[dev.cloudinit]
driver = "pci-virtio-block"
pci-path = "0.16.0"
block_dev = "cloudinit_be"

# Define the backend to that disk as the cloudinit type
[block_dev.cloudinit_be]
type = "cloudinit"

# Data from this cloudinit section will be used to populate the above block_dev
[cloudinit]
user-data = '''
#cloud-config
users:
- default
- name: test
  sudo: 'ALL=(ALL) NOPASSWD:ALL'
  lock_passwd: false
  hashed_passwd: '$6$rounds=4096$MBW/3OrwWLifnv30$QM.oCQ3pzV7X4EToX9IyZmplvaTgpZ6YJ50MhQrwlryj1soqBW5zvraVttYwfyWdxigHpZHTjY9kT.029UOEn1'
'''
# Instead of specifying string data like above, a path to a file can be used too:
# user-data-path = "path/to/file"

# Instance metadata is configured the same way:
# meta-data = "..."
# or
# meta-data-path = "path/to/file"

# Same with network configuration:
# network-config = "..."
# or
# network-config-path = "path/to/file"
```
