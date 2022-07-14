# Propolis Zone

This binary can be used to produce an Omicron-branded Zone image,
which consists of the Propolis Server binary (along
with some auxiliary files) in a specially-formatted tarball.

A manifest describing this Zone image exists in `package-manifest.toml`,
and the resulting image is created as `out/propolis-server.tar.gz`.

To create the Zone image:

```rust
$ cargo build --release
$ cargo run -p propolis-package
```
