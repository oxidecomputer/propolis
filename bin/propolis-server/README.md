# Propolis Server

The Propolis server binary provides a REST API to create and manage Propolis
VMs. It typically runs in the context of a complete Oxide deployment, where it
is operated by the sled agent, but it can also be run as a freestanding binary
for ad hoc testing and management of Propolis VMs.

## Running

The server binary requires a path to a [guest bootrom
image](../propolis-standalone#guest-bootrom) on the local filesystem. It also
must run with privileges sufficient to create `bhyve` virtual machines; the
`pfexec(1)` utility can help enable these privileges for sufficiently-privileged
users.

To build and run the server:

```
# cargo build --bin propolis-server
# pfexec target/debug/propolis-server <path_to_bootrom> <ip:port> <vnc_ip:port>
```

The API will be served on `ip:port`.

The easiest way to interact with the server is to use
[`propolis-cli`](../propolis-cli), but you can also interact directly with the
REST API using utilities like cURL. The server's [OpenAPI
specification](../../openapi/propolis-server.json) is checked into the repo.
