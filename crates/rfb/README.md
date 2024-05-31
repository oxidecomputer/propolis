# RFB

This crate implements a server-side implementation of the Remote Framebuffer
Protocol. Consumers of the crate can use the implementation while providing
their own framebuffer data by implementing the trait `rfb::server::Server`.

RFB is the protocol used to implement VNC. See [RFC
6143](https://www.rfc-editor.org/rfc/rfc6143.html) for details.

## Example Server

See the [example implementation](examples/server.rs) for a trivial
implementation.

To run the example, run:
```bash
$ cargo build --example example-server
$ ./target/debug/examples/example-server
```

Then connect to the VNC server with your favorite client (such as
[noVNC](https://github.com/novnc/noVNC)) at localhost:9000.
