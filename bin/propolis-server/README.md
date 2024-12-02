# Propolis Server

This binary provides a REST API to create and manage a Propolis VM. It typically
runs in the context of a complete Oxide control plane deployment, but it can
also be run as a freestanding binary for ad hoc testing of Propolis VMs.

## Running

The server requires a path to a [guest bootrom
image](../propolis-standalone#guest-bootrom) on the local filesystem. It also
must be run with privileges sufficient to create bhyve virtual machines. The
`pfexec(1)` utility can help enable these privileges.

To build and run the server:

```bash
cargo build --bin propolis-server
pfexec target/debug/propolis-server <path_to_bootrom> <ip:port> <vnc_ip:port>
```

The API will be served on `ip:port`. The easiest way to interact with the server
is to use [`propolis-cli`](../propolis-cli), but you can also use tools like
cURL to interact with the API directly. The server's OpenAPI specification is
[checked into the repo](../../openapi/propolis-server.json).
