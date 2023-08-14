# Propolis Server

## Running

Propolis is mostly intended to be used via a REST API to drive all of its
functionality. The standard `cargo build` will produce a `propolis-server`
binary you can run:

```
# propolis-server run <config_file> <ip:port>
```

Note that the server must run as root. One way to ensure propolis-server has
sufficient privileges is by using `pfexec(1)`, as such:

```
# pfexec propolis-server run <config_file> <ip:port>
```

## Example Configuration

**Note**: the goal is to move the device config from the toml to instead be
configured via REST API calls.

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

## Prerequisites

When running the server by hand, the appropriate bootrom is required to start
guests properly.  See the [standalone
documentation](../propolis-standalone#guest-bootrom) for more details.  Details
for [creating necessary vnics](../propolis-standalone#vnic) can be found there
as well, if exposing network devices to the guest.

## CLI Interaction

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
