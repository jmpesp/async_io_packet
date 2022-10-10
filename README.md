`AsyncIoPacket` is a layer on top of anything that implements Tokio's AsyncRead
and AsyncWrite, that itself implements AsyncRead and AsyncWrite. It will frame
data written through it into packets, and optionally perform a transform on the
packeted data.

Assuming that consumer code is written using the AsyncRead and AsyncWrite
traits instead of a specific object, `AsyncIoPacket` is meant to be dropped in
to any existing code. For example, add Fletcher32 checksum verification to your
connections like so:

```rust
// Server code
let listener = TcpListener::bind(addr).await?;
let (socket, _) = listener.accept().await?;
let socket = AsyncIoPacket::new(socket, ChecksumPacketDataTransform {});

// Client code
let stream = TcpStream::connect(addr).await?;
let stream = AsyncIoPacket::new(stream, ChecksumPacketDataTransform {});
```

Layering is also possible (and kinda the point!). For example,

```rust
// Server code
let listener = TcpListener::bind(addr).await?;
let (socket, _) = listener.accept().await?;
let socket = AsyncIoPacket::new(socket, ChecksumPacketDataTransform {});
let socket = AsyncIoPacket::new(socket, OtpPacketDataTransform::<rand::rngs::StdRng>::new(3283870128904943616) );
let socket = AsyncIoPacket::new(socket, ChecksumPacketDataTransform {});

// Client code
let stream = TcpStream::connect(addr).await?;
let stream = AsyncIoPacket::new(stream, ChecksumPacketDataTransform {});
let stream = AsyncIoPacket::new(stream, OtpPacketDataTransform::<rand::rngs::StdRng>::new(3283870128904943616) );
let stream = AsyncIoPacket::new(stream, ChecksumPacketDataTransform {});
```
