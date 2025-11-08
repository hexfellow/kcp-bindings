# KCP-Bindings

This repository is just a wrapper around the KCP library to make it easier to use in other rust projects.

All credits go to the original authors of the KCP library: https://github.com/skywind3000/kcp

## HexFellow Integration

When the `hexfellow` feature is enabled, this library will use the `tokio` crate to handle the UDP socket.


## TODO

1. Remove the `anyhow` dependency and use custom error types.
2. Use stream API.
