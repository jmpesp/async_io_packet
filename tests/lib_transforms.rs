// Copyright 2022 Oxide Computer Company

use anyhow::Result;
use rand::Rng;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use async_io_packet::AsyncIoPacket;
use async_io_packet::PacketDataTransform;

use async_io_packet::ChecksumPacketDataTransform;
use async_io_packet::NoopPacketDataTransform;
use async_io_packet::OtpPacketDataTransform;

use async_io_packet_test_utils::TestFramework;

async fn test_library_transform<T: PacketDataTransform + Send + 'static>(
    client: T,
    server: T,
) -> Result<()> {
    let mut tf = TestFramework::new(client, server).await?;

    let n = tf
        .client
        .write("test post please ignore".as_bytes())
        .await?;
    assert_eq!(n, "test post please ignore".len());

    let mut buf = vec![0u8; u16::MAX as usize];

    let n = tf.server.read(&mut buf).await?;

    assert_eq!(&buf[0..n], "test post please ignore".as_bytes(),);

    Ok(())
}

#[tokio::test]
async fn test_library_transforms() -> Result<()> {
    test_library_transform(NoopPacketDataTransform {}, NoopPacketDataTransform {}).await?;

    test_library_transform(
        OtpPacketDataTransform::<rand::rngs::StdRng>::new(3283870128904943616),
        OtpPacketDataTransform::<rand::rngs::StdRng>::new(3283870128904943616),
    )
    .await?;

    test_library_transform(
        ChecksumPacketDataTransform {},
        ChecksumPacketDataTransform {},
    )
    .await?;

    Ok(())
}

async fn test_library_transform_max_bytes<T: PacketDataTransform + Send + 'static>(
    client: T,
    server: T,
) -> Result<()> {
    let mut tf = TestFramework::new(client, server).await?;

    let _n = tf.client.write(&vec![0xFFu8; u16::MAX as usize]).await?;
    tf.client.flush().await?;

    let mut buf = vec![0u8; u16::MAX as usize];

    let n = tf.server.read_exact(&mut buf).await?;

    assert_eq!(n, u16::MAX as usize);

    assert_eq!(buf[0..n], vec![0xFFu8; u16::MAX as usize],);

    Ok(())
}

#[tokio::test]
async fn test_library_transforms_max_bytes() -> Result<()> {
    test_library_transform_max_bytes(NoopPacketDataTransform {}, NoopPacketDataTransform {})
        .await?;

    test_library_transform_max_bytes(
        OtpPacketDataTransform::<rand::rngs::StdRng>::new(6243752233550921728),
        OtpPacketDataTransform::<rand::rngs::StdRng>::new(6243752233550921728),
    )
    .await?;

    test_library_transform_max_bytes(
        ChecksumPacketDataTransform {},
        ChecksumPacketDataTransform {},
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_junk_data_if_otp_seed_mismatch() -> Result<()> {
    let client = OtpPacketDataTransform::<rand::rngs::StdRng>::new(50921728);
    let server = OtpPacketDataTransform::<rand::rngs::StdRng>::new(62372233);

    let mut tf = TestFramework::new(client, server).await?;

    let _n = tf.client.write(&vec![0xFFu8; 1024]).await?;
    tf.client.flush().await?;

    let mut buf = vec![0u8; 1024];
    let n = tf.server.read_exact(&mut buf).await?;

    assert_eq!(n, 1024);
    assert_ne!(vec![0xFFu8; 1024], buf);

    Ok(())
}

#[tokio::test]
async fn the_whole_point_is_layering_these_things() -> Result<()> {
    // create many layers of transforms, test data transiting through it

    // create the server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let server_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let server = AsyncIoPacket::new(
            AsyncIoPacket::new(
                AsyncIoPacket::new(
                    AsyncIoPacket::new(
                        AsyncIoPacket::new(socket, ChecksumPacketDataTransform {}),
                        OtpPacketDataTransform::<rand::rngs::StdRng>::new(6243752233550921728),
                    ),
                    ChecksumPacketDataTransform {},
                ),
                OtpPacketDataTransform::<rand::rngs::StdRng>::new(721716561223421579),
            ),
            ChecksumPacketDataTransform {},
        );
        Ok(server)
    });

    let client_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
        let stream = TcpStream::connect(addr).await?;
        let client = AsyncIoPacket::new(
            AsyncIoPacket::new(
                AsyncIoPacket::new(
                    AsyncIoPacket::new(
                        AsyncIoPacket::new(stream, ChecksumPacketDataTransform {}),
                        OtpPacketDataTransform::<rand::rngs::StdRng>::new(6243752233550921728),
                    ),
                    ChecksumPacketDataTransform {},
                ),
                OtpPacketDataTransform::<rand::rngs::StdRng>::new(721716561223421579),
            ),
            ChecksumPacketDataTransform {},
        );
        Ok(client)
    });

    let mut client = client_join_handle.await??;
    let mut server = server_join_handle.await??;

    // client -> server

    let _n = client.write("hey, this works nicely!".as_bytes()).await?;
    client.flush().await?;

    let mut buf = vec![0u8; 1024];
    let n = server.read(&mut buf).await?;

    assert_eq!(&buf[0..n], "hey, this works nicely!".as_bytes());

    // server -> client

    let _n = server
        .write("yes, in this direction too!".as_bytes())
        .await?;
    server.flush().await?;

    let n = client.read(&mut buf).await?;
    assert_eq!(&buf[0..n], "yes, in this direction too!".as_bytes());

    // 100K write from client -> server

    let expected = {
        let mut buf = vec![0u8; 100 * 1024];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let _n = client.write(&expected).await?;
    client.flush().await?;

    // read 100K

    let mut actual = vec![0u8; 100 * 1024];
    server.read_exact(&mut actual).await?;

    assert_eq!(expected, actual);

    Ok(())
}
