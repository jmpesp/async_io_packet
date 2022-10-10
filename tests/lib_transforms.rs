// Copyright 2022 Oxide Computer Company

use std::marker::Unpin;

use anyhow::Result;
use rand::Rng;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use async_io_packet::PacketDataTransform;

use async_io_packet::ChecksumPacketDataTransform;
use async_io_packet::NoisePacketDataTransform;
use async_io_packet::NoopPacketDataTransform;
use async_io_packet::OtpPacketDataTransform;

use async_io_packet_test_utils::TestFramework;

async fn test_library_transform<T: PacketDataTransform + Send + 'static>(
    client: T,
    server: T,
) -> Result<()> {
    let mut tf = TestFramework::new(client, server).await?;

    // Send and receive a small message
    let n = tf
        .client
        .write("test post please ignore".as_bytes())
        .await?;
    assert_eq!(n, "test post please ignore".len());

    let mut buf = vec![0u8; u16::MAX as usize];
    let n = tf.server.read(&mut buf).await?;
    assert_eq!(&buf[0..n], "test post please ignore".as_bytes());

    // Send and receive a giant message
    let _n = tf.server.write(&vec![0xFFu8; u16::MAX as usize]).await?;
    tf.server.flush().await?;

    let mut buf = vec![0u8; u16::MAX as usize];
    let n = tf.client.read_exact(&mut buf).await?;

    assert_eq!(n, u16::MAX as usize);
    assert_eq!(buf[0..n], vec![0xFFu8; u16::MAX as usize]);

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

    let n = tf.client.write(&vec![0xFFu8; u16::MAX as usize]).await?;
    assert_eq!(n, u16::MAX as usize);
    tf.client.flush().await?;

    let mut buf = vec![0u8; u16::MAX as usize];
    let n = tf.server.read_exact(&mut buf).await?;
    assert_eq!(n, u16::MAX as usize);
    assert_eq!(buf[0..n], vec![0xFFu8; u16::MAX as usize]);

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

    let n = tf.client.write(&vec![0xFFu8; 1024]).await?;
    assert_eq!(n, 1024);
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
    let tf = TestFramework::new_with_layers(
        [
            Box::new(ChecksumPacketDataTransform {}) as Box<dyn PacketDataTransform + Send>,
            Box::new(OtpPacketDataTransform::<rand::rngs::StdRng>::new(
                6243752233550921728,
            )),
            Box::new(ChecksumPacketDataTransform {}),
            Box::new(OtpPacketDataTransform::<rand::rngs::StdRng>::new(
                721716561223421579,
            )),
            Box::new(ChecksumPacketDataTransform {}),
        ]
        .into_iter(),
        [
            Box::new(ChecksumPacketDataTransform {}) as Box<dyn PacketDataTransform + Send>,
            Box::new(OtpPacketDataTransform::<rand::rngs::StdRng>::new(
                6243752233550921728,
            )),
            Box::new(ChecksumPacketDataTransform {}),
            Box::new(OtpPacketDataTransform::<rand::rngs::StdRng>::new(
                721716561223421579,
            )),
            Box::new(ChecksumPacketDataTransform {}),
        ]
        .into_iter(),
    )
    .await?;

    let (mut client, mut server) = tf.consume();

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

async fn client_action<T: AsyncRead + AsyncWrite + Unpin>(mut client: T) -> Result<()> {
    // Send 1024 bytes
    let n = client.write(&vec![0xFFu8; 1024]).await?;
    assert_eq!(n, 1024);
    client.flush().await?;

    // Receive max bytes
    let mut buf = vec![0u8; u16::MAX as usize];
    let n = client.read_exact(&mut buf).await?;
    assert_eq!(n, u16::MAX as usize);
    assert_eq!(buf[0..n], vec![0xFFu8; u16::MAX as usize]);

    Ok(())
}

async fn server_action<T: AsyncRead + AsyncWrite + Unpin>(mut server: T) -> Result<()> {
    // Receive 1024 bytes
    let mut buf = vec![0u8; 1024];
    let n = server.read_exact(&mut buf).await?;
    assert_eq!(n, 1024);
    assert_eq!(vec![0xFFu8; 1024], buf);

    // Send max bytes
    let n = server.write(&vec![0xFFu8; u16::MAX as usize]).await?;
    assert_eq!(n, u16::MAX as usize);
    server.flush().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_noise_basic() -> Result<()> {
    let client = NoisePacketDataTransform::client(&[255u8; 32])?;
    let server = NoisePacketDataTransform::server(&[255u8; 32])?;

    let tf = TestFramework::new(client, server).await?;

    let (client, server) = tf.consume();

    let server_join_handle = tokio::spawn(server_action(server));
    let client_join_handle = tokio::spawn(client_action(client));

    server_join_handle.await??;
    client_join_handle.await??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_noise_with_checksum() -> Result<()> {
    let key = {
        let mut buf = vec![0u8; 32];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let tf = TestFramework::new_with_layers(
        [
            Box::new(NoisePacketDataTransform::client(&key)?)
                as Box<dyn PacketDataTransform + Send>,
            Box::new(ChecksumPacketDataTransform {}),
        ]
        .into_iter(),
        [
            Box::new(NoisePacketDataTransform::server(&key)?)
                as Box<dyn PacketDataTransform + Send>,
            Box::new(ChecksumPacketDataTransform {}),
        ]
        .into_iter(),
    )
    .await?;

    let (client, server) = tf.consume();

    let server_join_handle = tokio::spawn(server_action(server));
    let client_join_handle = tokio::spawn(client_action(client));

    server_join_handle.await??;
    client_join_handle.await??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_double_noise_all_the_way_across_the_sky() -> Result<()> {
    let key1 = {
        let mut buf = vec![0u8; 32];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let key2 = {
        let mut buf = vec![0u8; 32];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let tf = TestFramework::new_with_layers(
        [
            Box::new(NoisePacketDataTransform::client(&key1)?)
                as Box<dyn PacketDataTransform + Send>,
            Box::new(NoisePacketDataTransform::client(&key2)?),
        ]
        .into_iter(),
        [
            Box::new(NoisePacketDataTransform::server(&key1)?)
                as Box<dyn PacketDataTransform + Send>,
            Box::new(NoisePacketDataTransform::server(&key2)?),
        ]
        .into_iter(),
    )
    .await?;

    let (client, server) = tf.consume();

    let server_join_handle = tokio::spawn(server_action(server));
    let client_join_handle = tokio::spawn(client_action(client));

    server_join_handle.await??;
    client_join_handle.await??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_its_starting_to_look_like_triple_noise() -> Result<()> {
    let key1 = {
        let mut buf = vec![0u8; 32];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let key2 = {
        let mut buf = vec![0u8; 32];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let key3 = {
        let mut buf = vec![0u8; 32];
        let mut rng = rand::thread_rng();
        rng.fill(&mut buf[..]);
        buf
    };

    let tf = TestFramework::new_with_layers(
        [
            Box::new(NoisePacketDataTransform::client(&key1)?)
                as Box<dyn PacketDataTransform + Send>,
            Box::new(NoisePacketDataTransform::client(&key2)?),
            Box::new(NoisePacketDataTransform::client(&key3)?),
        ]
        .into_iter(),
        [
            Box::new(NoisePacketDataTransform::server(&key1)?)
                as Box<dyn PacketDataTransform + Send>,
            Box::new(NoisePacketDataTransform::server(&key2)?),
            Box::new(NoisePacketDataTransform::server(&key3)?),
        ]
        .into_iter(),
    )
    .await?;

    let (client, server) = tf.consume();

    let server_join_handle = tokio::spawn(server_action(server));
    let client_join_handle = tokio::spawn(client_action(client));

    server_join_handle.await??;
    client_join_handle.await??;

    Ok(())
}
