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
### Building `propolis-standalone`
- Clone this repository on an illumos box (e.g. `atrium`)
- In that folder, run
  `cargo build --release -ppropolis-standalone --features=crucible`
- This will produce a `propolis-standalone` binary in `target/release/`
- Copy this binary to your target Gimlet

### Building `crucible`
- Clone [`oxidecomputer/crucible`](https://github.com/oxidecomputer/crucible) on
  an illumos box (e.g. `atrium`)
- In that folder, run `cargo build --release -pcrucible-downstairs -pdsc`
- This will produce `crucible-downstairs` and `dsc` binaries in `target/release`
- Copy those files to your target Gimlet

### VM stuff
See the [`propolis-standalone` README](../bin/propolis-standalone/README.md)
for details on how to get
  * VM Image file
  * VM OVMF file

Copy those files to your target Gimlet.

## Instructions

### Onetime setup on the Gimlet.

Setup for a virtual NIC to be used by the VM.

```
dladm create-vnic -t -l igb0 -m 02:08:20:ac:e9:16 vnic_prop0
```

Setup of a zpool on three SSDs.

Crucible downstairs runs on top of a filesystem (ZFS in our case).
On your bench Gimlet, you should select three NVMe disks, and create a zpool
on each of them.  You can use an existing zpool.

If you're creating new zpools, start by running `format` to list disk names:
```
BRM42220012 # format
Searching for disks...done


AVAILABLE DISK SELECTIONS:
       0. c1t00A0750130082207d0 <NVMe-Micron_7300_MTFDHBG1T9TDF-95420260-1.75TB>
          /pci@0,0/pci1de,fff9@1,3/pci1344,3100@0/blkdev@w00A0750130082207,0
       1. c2t0014EE81000BC481d0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@0,0/pci1de,fff9@3,2/pci1b96,0@0/blkdev@w0014EE81000BC481,0
       2. c3t0014EE81000BC783d0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@0,0/pci1de,fff9@3,3/pci1b96,0@0/blkdev@w0014EE81000BC783,0
       3. c4t0014EE81000BC78Fd0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@0,0/pci1de,fff9@3,4/pci1b96,0@0/blkdev@w0014EE81000BC78F,0
       4. c5t0014EE81000BC37Dd0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@38,0/pci1de,fff9@1,2/pci1b96,0@0/blkdev@w0014EE81000BC37D,0
       5. c6t0014EE81000BC28Ad0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@38,0/pci1de,fff9@1,3/pci1b96,0@0/blkdev@w0014EE81000BC28A,0
       6. c7t00A0750130082248d0 <NVMe-Micron_7300_MTFDHBG1T9TDF-95420260-1.75TB>
          /pci@38,0/pci1de,fff9@3,3/pci1344,3100@0/blkdev@w00A0750130082248,0
       7. c8t0014EE81000BC39Bd0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@ab,0/pci1de,fff9@1,1/pci1b96,0@0/blkdev@w0014EE81000BC39B,0
       8. c9t0014EE81000BC3C8d0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@ab,0/pci1de,fff9@1,2/pci1b96,0@0/blkdev@w0014EE81000BC3C8,0
       9. c10t0014EE81000BC4CCd0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@ab,0/pci1de,fff9@1,3/pci1b96,0@0/blkdev@w0014EE81000BC4CC,0
      10. c11t0014EE81000BC786d0 <NVMe-WUS4C6432DSP3X3-R2210000-2.91TB>
          /pci@ab,0/pci1de,fff9@1,4/pci1b96,0@0/blkdev@w0014EE81000BC786,0`
Specify disk (enter its number): ^C
```

Then, create zpools with your desired serial names, e.g.
```
zpool create -f -o ashift=12 -O atime=off -m /pool/disk0 cru0 c1t00A0750130082207d0
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
regions where your data will live.  The values you specify here for
extent_size, extent_count, and block_size will decide how big your region
is as well as can change the performance characteristics of the region.

Omicron's current defaults are to have a 64 MiB extent size:
```
pub const EXTENT_SIZE: u64 = 64_u64 << 20;
```

Which ends up like this:
For 512 byte blocks, 131072 is the extent size.
For 4096 byte blocks, 16384 is the extent size.

```
./target/release/dsc create \
  --ds-bin ./target/release/crucible-downstairs \
  --cleanup \
  --extent-size 16384 \
  --extent-count 512 \
  --block-size 4096 \
  --encrypted \
  --region-dir /pool/disk0/region \
  --region-dir /pool/disk1/region \
  --region-dir /pool/disk2/region
```

(modify the `dsc` and `crucible-downstairs` paths based on where you put those
binaries)

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

### Start `propolis-standalone`

To start `propolis-standalone`, you'll need a configuration file.  The specifics
will depend on file paths, image type, etc.

Here's an example TOML file, assuming you have used the above settings for block
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

# Create your own key (openssl rand -base64 32) Or use this.
encryption_key = "tCw7zw0hAsPuxMOTWwnPEFYjBK9qJRtYyGdEXKEnrg0="

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
