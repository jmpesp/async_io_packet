// Copyright 2022 Oxide Computer Company

use std::marker::Send;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

use async_io_packet::AsyncIoPacket;
use async_io_packet::PacketDataTransform;

// Trait + blanket impl
pub trait AsyncIO: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIO for T {}

pub struct TestFramework {
    pub client: Box<dyn AsyncIO>,
    pub server: Box<dyn AsyncIO>,
}

impl TestFramework {
    pub async fn new<U>(client_transform: U, server_transform: U) -> Result<Self>
    where
        U: PacketDataTransform + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.clone();

        let server_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let server = AsyncIoPacket::new(socket, server_transform);
            Ok(Box::new(server))
        });

        let client_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await?;
            let client = AsyncIoPacket::new(stream, client_transform);
            Ok(Box::new(client))
        });

        let client = client_join_handle.await??;
        let server = server_join_handle.await??;

        Ok(Self {
            client,
            server,
        })
    }

    pub async fn new_with_layers(
        mut client_transforms: impl Iterator<Item = Box<dyn PacketDataTransform + Send + 'static>> + Send + 'static,
        mut server_transforms: impl Iterator<Item = Box<dyn PacketDataTransform + Send + 'static>> + Send + 'static,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.clone();

        let server_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;

            let mut server: Box<dyn AsyncIO> = Box::new(socket);
            while let Some(transform) = server_transforms.next() {
                server = Box::new(
                    AsyncIoPacket::<Box<dyn AsyncIO>, Box<dyn PacketDataTransform>>::new(server, transform)
                );
            }

            Ok(server)
        });

        let client_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await?;

            let mut client: Box<dyn AsyncIO> = Box::new(stream);
            while let Some(transform) = client_transforms.next() {
                client = Box::new(
                    AsyncIoPacket::<Box<dyn AsyncIO>, Box<dyn PacketDataTransform>>::new(client, transform)
                );
            }

            Ok(client)
        });

        let client = client_join_handle.await??;
        let server = server_join_handle.await??;

        Ok(Self {
            client,
            server,
        })
    }

    pub fn consume(self) -> (Box<dyn AsyncIO>, Box<dyn AsyncIO>) {
        (self.client, self.server)
    }
}
