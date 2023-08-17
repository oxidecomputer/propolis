# How to use the VCR replacement endpoint in propolis-server

This document describes how to use the `/instance/disk/{id}/vcr` API endpoint
to replace a downstairs of a Crucible volume attached to a Propolis instance.
We will use both propolis-server and propolis-cli to do this.

## Setup

You will need:
 * `Propolis-server` and `propolis-cli` binaries
 * A bootable VM image file
 * The OVMF file
 * A server.toml file for propolis-server
 * A crucible-disks file for crucible configuration.
 * A VCR replace json file.
 * A copy of the binaries `crucible-downstairs` and `dsc`, these should
   match what version Propolis expects.

## Start with crucible downstairs

In one window, run `dsc` to create and start four downstairs on four
different regions.

`--ds-bin` tells `dsc` where to find the `crucible-downstairs` binary.

The `--extent_size`, `--extent_count`, and `--block_size` values here are all
used in later config files and they must match otherwise crucible will
fail to start.

```
dsc start --create --cleanup --extent-size 16384 --extent-count 128 --block-size 4096 --region-count 4 --ds-bin ./target/release/crucible-downstairs
```

## Start propolis-server

In another window, start propolis server.
I used this toml file.  You will need to change the paths to your OVMF file
and your bootable VM image file.
```
bootrom = "/home/alan/vm/OVMF_CODE.fd"

[block_dev.ubuntu]
type = "file"
path = "/home/alan/vm/large-focal.raw"

[dev.block0]
driver = "pci-nvme"
block_dev = "ubuntu"
pci-path = "0.4.0"
```

To start the server, run this:

```
pfexec ./target/release/propolis-server run server.toml 127.0.0.1:55400
```

Leave this window running, it will show output from propolis and crucible.

## Create the VM with a crucible disk:

Next we create a crucible NVMe disk using `propolis-cli`.

Here is an example crucible-disks.json file.  Note the values you used above
for dsc need to match what is in this file.
```
[
{
    "device": "nvme",
    "name": "block2",
    "read_only": false,
    "slot": 3,
    "volume_construction_request": {
        "type": "volume",
        "block_size": 4096,
        "id": "0cedae45-3d6e-4d90-b2cb-56f1a1a42a89",
        "read_only_parent": null,
        "sub_volumes": [
            {
                "type": "region",
                "block_size": 4096,
                "blocks_per_extent": 16384,
                "extent_count": 128,
                "gen": 1,
                "opts": {
                    "cert_pem": null,
                    "control": null,
                    "flush_timeout": null,
                    "id": "0cedae45-3d6e-4d90-b2cb-56f1a1a42a89",
                    "key": null,
                    "key_pem": null,
                    "lossy": false,
                    "read_only": false,
                    "root_cert_pem": null,
                    "target": ["127.0.0.1:8810",
                             "127.0.0.1:8820",
                             "127.0.0.1:8830"
                    ]
                }
            }
        ]
    }
}
]
```

Using a third window, create the VM and add the crucible disk like this:

```
propolis-cli -s 127.0.0.1 -p 55400 new crub --crucible-disks crucible-disks.json
```

Then, start the VM:
```
propolis-cli -s 127.0.0.1 -p 55400 state run
```

## Replace a downstairs.

To replace a downstairs, we are using the almost same VCR that we used
to create our crucible disk, but with two things different.
1. The generation number has increased by one.
2. One (and only one) of the `target`s has changed to a different IP:Port.

The `.json` file we use for this looks similar to our previous one, here
is an example file:

```
{
    "name": "block2",
    "vcr": {
        "type": "volume",
        "block_size": 4096,
        "id": "0cedae45-3d6e-4d90-b2cb-56f1a1a42a89",
        "read_only_parent": null,
        "sub_volumes": [
            {
                "type": "region",
                "block_size": 4096,
                "blocks_per_extent": 16384,
                "extent_count": 128,
                "gen": 2,
                "opts": {
                    "cert_pem": null,
                    "control": null,
                    "flush_timeout": null,
                    "id": "0cedae45-3d6e-4d90-b2cb-56f1a1a42a89",
                    "key": null,
                    "key_pem": null,
                    "lossy": false,
                    "read_only": false,
                    "root_cert_pem": null,
                    "target": ["127.0.0.1:8810",
                             "127.0.0.1:8820",
                             "127.0.0.1:8840"
                    ]
                }
            }
        ]
    }
}
```

You might notice in this second file, our generation number has gone up one,
and `target[2]` is different.  Send this replace.json file over to the server
with this propolis-cli command:

```
propolis-cli -s 127.0.0.1 -p 55400 vcr -u 0cedae45-3d6e-4d90-b2cb-56f1a1a42a89 --vcr-replace ./replace.json
```

This should result in more messages on the propolis-server window, and,
eventually, a new downstairs.
