# Running a live migration "by hand" with a crucible boot disk

In the product, live migration is managed by nexus. Still, it is extremely
useful for development to be able to test software components in isolation. One
obstacle for testing inter-machine migration in propolis without a full control
plane is the need for shared storage — in particular, a source of shared storage
for the guest's boot disk.

Since crucible will be providing storage in the product, I chose to get
that working over other options. This document has some instructions on how to
get propolis to use crucible as a backend for a boot disk. At the moment it's
not the most user-friendly experience, but since it took some effort to figure
out, I wanted to at least capture what I did.

## Requirements

For this setup, you'll need:
- a "source" propolis server
- a "destination" propolis server (on the same machine or otherwise)
- a propolis CLI
- a copy of [crucible](https://github.com/oxidecomputer/crucible) and a place to
  run downstairs processes that the source/destination machines can access
- an OS image that you'd like to boot

## Setup

### Seed crucible downstairs with the OS image

From the machine where you will run the crucible downstairs, set up 3 crucible
downstairs regions. Use  the `--import-path` flag to specify where the OS image
is on the filesystem. Specify the address and port the downstairs will listen
on using the `-a` and the `-p` flags, respectively. The address may be
`localhost` or the external IP address of the machine where the downstairs is
running. Note that the IP:port specification will be used again later in the
JSON file that gets passed to propolis.

For example:
```
$ ./target/release/crucible-downstairs create --import-path /home/jordan/images/helios-generic-ttya-base_20230109.raw --data region8810 --uuid $(uuidgen) --extent-size 64000 --extent-count 64

$ ./target/release/crucible-downstairs create --import-path /home/jordan/images/helios-generic-ttya-base_20230109.raw --data region8820 --uuid $(uuidgen) --extent-size 64000 --extent-count 64

$ ./target/release/crucible-downstairs create --import-path /home/jordan/images/helios-generic-ttya-base_20230109.raw --data region8830 --uuid $(uuidgen) --extent-size 64000 --extent-count 64
```

Each `create` will setup a region file. In the above example, these files are
`region8810`, `region8820`, and `region8830`, respectively.

### Run the crucible downstairs

After seeding the downstairs with the image, run the downstairs processes.

For example:
```
$ ./target/release/crucible-downstairs run -d region8810 -p 8810 -a 172.20.3.73
$ ./target/release/crucible-downstairs run -d region8820 -p 8820 -a 172.20.3.73
$ ./target/release/crucible-downstairs run -d region8830 -p 8830 -a 172.20.3.73
```

### Create a JSON file with disk requests

Now that we've got a crucible volume setup, we need to configure propolis to be
aware of it as a backend. One can do this by passing the `--crucible-disks`
flag and a JSON file of an array of `DiskRequest`s  when creating or migrating
a VM.

On the source machine, create a JSON file like this:

```
[
{
    "device": "virtio",
    "name": "helios-blockdev",
    "read_only": false,
    "slot": 1,
    "volume_construction_request": {
        "type": "volume",
        "block_size": 512,
        "id": "0cedae45-3d6e-4d90-b2cb-56f1a1a42a89",
        "read_only_parent": null,
        "sub_volumes": [
            {
                "type": "region",
                "block_size": 512,
                "blocks_per_extent": 64000,
                "extent_count": 64,
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
                    "target": ["172.20.3.73:8810",
                             "172.20.3.73:8820",
                             "172.20.3.73:8830"
                    ]
                }
            }
        ]
    }
}
]
```

Several fields in this file must match the parameters specified when the
crucible downstairs processes were created, specifically: `block_size` (note
that it occurs twice in the JSON file), `blocks_per_extent`, and
`extent_count`. The `target` field is an array of IP:port addresses where the
downstairs are expected to be running.

One important thing to know is that the generation number field (`gen`) must be
bumped manually each time a VM is created (or migrated). (In the product, the
generation number is tracked by nexus.) A fresh crucible downstairs will start
with generation number 1.

To see the current generation number of a downstairs, you can dump the region
and check the highest generation number. Use the `-d` flag to select the
directory containing the region:

```
$ ./target/debug/crucible-downstairs dump -d region8810
EXT          BLOCKS GEN0   FL0  D0
  0 0000000-0063999   10   608   F
  1 0064000-0127999    0     1   F
  2 0128000-0191999    0     1   F
  3 0192000-0255999    0     1   F

... (output elided)

Max gen: 11,  Max flush: 642
```

You will need to use the max generation number of all 3 downstairs.

### Create the VM on the source server

On the source machine, run the propolis server with whatever TOML configuration
you desire, except for the boot disk, which will be specified through the API.

Create the VM using the `--crucible-disks` flag and the JSON file. For example:
```
$ ./target/debug/propolis-cli -s 172.20.3.73 -p 8000 new --crucible-disks disks.json vm0
```

Run the VM:
```
$ ./target/debug/propolis-cli -s 172.20.3.73 -p 8000 state run
```

You may wish to watch the console to make sure it boots:
```
$ ./target/debug/propolis-cli -s 172.20.3.73 -p 8000 serial
```

### Migrate the VM to the destination server

Now it's time to migrate the VM. The destination server will need to have the
same instance spec as the source server, so run the destination server with the
same TOML configuration as the source server. Similarly, the destination server
will need to know about the crucible backend. Like with the `create` command, we
can tell the destination server about this disk via request with the `migrate`
command and the `crucible-disks` flag.

Ensure the destination server is running. Make a copy of the JSON file you
created above and increment the generation number. Then, from the source, run
something like:
```
$ ./target/debug/propolis-cli -s 172.20.3.73 -p 8000 migrate 172.20.3.71 -p 8000 --crucible-disks disks2.json
```

If successful, you should be able to run the VM and see the serial console on
the destination side.


