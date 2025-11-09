# KCP-Bindings

This repository is just a wrapper around the KCP library to make it easier to use in other rust projects.

All credits go to the original authors of the KCP library: https://github.com/skywind3000/kcp

## HexFellow Integration

When the `hexfellow` feature is enabled, this library will use the `tokio` crate to handle the UDP socket.

### HexSocket

Made of header and data.

Header in little endian, 4 Bytes

- Byte[0]: 
  - Higher 4 Bits reserved. Always 0b1000. 
  - Lower 4 Bits is opcode just like websocket. 
    - 0x0: Continuation frame(Not used for now, all frames are single frame now)
    - 0x1: Text frame
    - 0x2: Binary frame
    - 0x8: Connection close(Not used for now)
    - 0x9: Ping
    - 0xA: Pong
- Byte[1]: Reserved. Always 0b0000_0000.
- Byte[2] and Byte[3]: Length of the data in u16.


## TODO

1. Remove the `anyhow` dependency and use custom error types.
2. Use stream API.
