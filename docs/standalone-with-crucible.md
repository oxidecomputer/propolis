# Run propolis-standalone with crucible disks

This document serves as an overview for running propolis-standalone using
crucible disks.

## Background: Why standalone?

In the product, the userspace VMM component for instances is
[propolis-server](../bin/propolis-server), which exposes API endpoints for the
rest of the control plane to manage instances. One can run propolis-server in
isolation, such as for development, and interact with its endpoints using a
client such as the [propolis CLI](../bin/propolis-cli).


The [standalone](../bin/propolis-standalone) version of propolis is useful
development tool to a VM up and running quickly without having to hit any API
endpoints. It takes an input a single TOML file, sets up a unix domain socket
that is connected to a VM's uart, and starts the guests when the user connects
to the socket. It also cleans up the VM gracefully when the user sends CTRL+C
to the running `propolis-standalone` program.

Beyond these differences, there are some differences in the state machine
related to the instance lifecycle as well as the emulation.

TODO: flesh out more of these differences, and maybe capture them in a
higher-level README.

## Requirements

TODO: document the following

- How to build each of propolis and crucible
- Assuming we are running on bench gimlets, which files to copy
  * propolis-standalone
  * crucible binaries
  * VM Image file
  * VM OVMF file
  * standalone.toml

## Instructions

### Copy over the required files.

TODO:

### Onetime setup on the gimlet.

Setup for a virtual NIC to be used by the VM.

```
dladm create-vnic -t -l igb0 -m 02:08:20:ac:e9:16 vnic_prop0
```

Setup of a zpool on three SSDs.

Crucible downstairs runs on top of a filesystem (ZFS in our case).
On your bench gimlet, you should select three NVMe disks, and create a zpool
on each of them.  You can use an existing zpool.

```
TODO: zfs pool create commands
```

On each zpool, create a directory where the crucible downstairs will live:
In our case, the pools are mounted at `/pool/disk1`, `/pool/disk2`, and
`/pool/disk3`.

```
mkdir /pool/disk0/region
mkdir /pool/disk1/region
mkdir /pool/disk2/region
```

### Create the crucible downstairs regions

With three pools created, you can now create the three downstairs crucible
regions where your data will live:

```
./target/release/dsc create \
  --ds-bin ./target/release/crucible-downstairs \
  --cleanup \
  --extent-size 131072 \
  --extent-count 128 \
  --block-size 4096 \
  --region-dir /pool/disk0/region \
  --region-dir /pool/disk1/region \
  --region-dir /pool/disk2/region
```

### Run the three downstairs

Once the regions are created, you can start the three downstairs using the
`dsc` command.

```
./target/release/dsc start \
  --ds-bin ./target/release/crucible-downstairs \
  --region-dir /pool/disk0/region \
  --region-dir /pool/disk1/region \
  --region-dir /pool/disk2/region
```

### Start propolis-standalone

standalone toml example
This example file assumes you have used the above settings for block
size, extent size, and extent count.

```toml
[main]
name = "testvm"
cpus = 4
bootrom = "/tmp/OVMF_CODE.fd"
memory = 2048

[block_dev.ubuntu]
type = "file"
path = "/tmp/large-focal.raw"

[dev.block0]
driver = "pci-nvme"
block_dev = "ubuntu"
pci-path = "0.4.0"

[block_dev.my_crucible]
type = "crucible"
# these MUST match the region configuration downstairs
block_size = 4096
blocks_per_extent = 131072
extent_count = 128
targets = [
  "127.0.0.1:8810",
  "127.0.0.1:8820",
  "127.0.0.1:8830",
]
generation = 5
upstairs_id = "e4396bd0-ede1-48d7-ac14-3d2094dfba5b"

[dev.block1]
driver = "pci-nvme"
block_dev = "my_crucible"
pci-path = "0.5.0"

[dev.net0]
driver = "pci-virtio-viona"
vnic = "vnic_prop0"
pci-path = "0.6.0"
```

Start propolis-standalone like this:

```
propolis-standalone standalone.toml
```
