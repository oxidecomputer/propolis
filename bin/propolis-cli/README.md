# Propolis CLI

The `propolis-cli` utility provides a user-friendly frontend to the
[`propolis-server`](../propolis-server) REST API.

## Getting started

The easiest way to launch a VM via the CLI is to write a TOML file describing
the VM's configuration. An example of such a file might be the following:

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

To create and run a Propolis VM using this configuration:

```
# propolis-cli -s <server ip> -p <port> new --config-toml <path> <VM name>
# propolis-cli -s <server ip> -p <port> state run
```

To connect to the VM's serial console:

```
# propolis-cli -s <server ip> -p <port> serial
```

Run `propolis-cli --help` to see the full list of supported commands and their
arguments.
