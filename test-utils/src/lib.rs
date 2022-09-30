// Copyright 2022 Oxide Computer Company

use std::marker::Send;

use anyhow::Result;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use async_io_packet::AsyncIoPacket;
use async_io_packet::PacketDataTransform;

pub struct TestFramework<U: PacketDataTransform + Send> {
    pub client: AsyncIoPacket<TcpStream, U>,
    pub server: AsyncIoPacket<TcpStream, U>,
}

impl<U: PacketDataTransform + Send + 'static> TestFramework<U> {
    pub async fn new(client_transform: U, server_transform: U) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.clone();

        let server_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let server = AsyncIoPacket::new(socket, server_transform);
            Ok(server)
        });

        let client_join_handle: JoinHandle<Result<_>> = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await?;
            let client = AsyncIoPacket::new(stream, client_transform);
            Ok(client)
        });

        let client = client_join_handle.await??;
        let server = server_join_handle.await??;

        Ok(Self {
            client,
            server,
        })
    }
}

