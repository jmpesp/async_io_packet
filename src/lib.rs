// Copyright 2022 Oxide Computer Company

use core::pin::Pin;
use core::task::Context;
use core::task::Poll;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::marker::Unpin;

use anyhow::bail;
use anyhow::Result;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

mod noop_transform;
pub use noop_transform::*;

mod otp_transform;
pub use otp_transform::*;

mod checksum_transform;
pub use checksum_transform::*;

pub trait PacketDataTransform: Unpin {
    /// Read received packeted data, optionally apply a transform, and return
    /// a payload buffer.
    fn read_payload(&mut self, msg: &[u8], cx: &mut Context<'_>) -> Result<Vec<u8>>;

    /// Read a payload, optionally apply a transform, and return a message that
    /// will eventually be sent as packet data.
    fn write_message(&mut self, payload: &[u8], cx: &mut Context<'_>) -> Result<Vec<u8>>;
}

/// AsyncIoPacket is a layer on top of some object that implements AsyncRead and
/// AsyncWrite that will perform a transformation of some kind.
pub struct AsyncIoPacket<T: AsyncRead + AsyncWrite + Unpin, U: PacketDataTransform> {
    /// AsyncRead + AsyncWrite object that AsyncIoPacket layers on top of
    io: T,

    /// perform some transform when reading and writing
    transform: U,

    /// a container for packets. the last packet in this list may be incomplete
    read_packet_buf: VecDeque<Vec<u8>>,

    /// remaining transformed bytes that need to be flushed
    remaining: Vec<u8>,

    // for writes
    write_packet_buf: Vec<Vec<u8>>,
}

// Each packet has a maximum size of 65536 bytes.
const MAX_FRAME_LEN: usize = u16::MAX as usize;

// The first bytes store the data length
const DATA_LEN: usize = 2;

// The header is the data length, plus a continuation byte
const HEADER_LEN: usize = DATA_LEN + 1;
const CONTINUATION_BYTE_OFFSET: usize = DATA_LEN;

const MAX_DATA_LEN: usize = MAX_FRAME_LEN - HEADER_LEN;

impl<T: AsyncRead + AsyncWrite + Unpin, U: PacketDataTransform> AsyncIoPacket<T, U> {
    pub fn new(io: T, transform: U) -> Self {
        Self {
            io,
            transform,
            read_packet_buf: VecDeque::new(),
            remaining: vec![],
            write_packet_buf: vec![],
        }
    }

    fn poll_write_packets_to_io(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<usize>> {
        if self.write_packet_buf.is_empty() {
            // buffer is empty
            return Poll::Ready(Ok(0));
        }

        // push out all packets in one big write
        let mut big_write = vec![0u8; self.write_packet_buf.iter().map(|x| x.len()).sum()];
        let mut i = 0;
        for packet in &self.write_packet_buf {
            big_write[i..(i + packet.len())].copy_from_slice(&packet[..]);
            i += packet.len();
        }
        assert_eq!(i, big_write.len());

        match Pin::new(&mut self.io).poll_write(cx, &big_write) {
            Poll::Pending => {
                // If the underlying io object returns Pending, we have to
                // propagate it up.
                Poll::Pending
            }

            Poll::Ready(Ok(mut bytes_written)) => {
                let bytes_written_copy = bytes_written;

                if bytes_written == i {
                    // for a totally successful write, clear write_packet_buf
                    self.write_packet_buf.clear();
                } else {
                    // on a partially successful write, consume whatever bytes got written.
                    while !self.write_packet_buf.is_empty()
                        && bytes_written >= self.write_packet_buf[0].len()
                    {
                        let popped = self.write_packet_buf.pop().unwrap();
                        bytes_written -= popped.len();
                    }

                    // truncate into middle of last one
                    if !self.write_packet_buf.is_empty() && bytes_written > 0 {
                        assert!(bytes_written < self.write_packet_buf[0].len());
                        let (_, remaining) = self.write_packet_buf[0].split_at(bytes_written);
                        self.write_packet_buf[0] = remaining.to_vec();
                    }
                }

                // notify the executor that this is ready to run, again, because
                // we have buffered packets
                if !self.write_packet_buf.is_empty() {
                    cx.waker().wake_by_ref();
                }

                Poll::Ready(Ok(bytes_written_copy))
            }

            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin, U: PacketDataTransform> AsyncRead for AsyncIoPacket<T, U> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut_self = self.get_mut();

        let mut input_bytes = vec![0u8; MAX_FRAME_LEN];
        let mut input_buf = tokio::io::ReadBuf::new(&mut input_bytes);

        // fill input buf by polling the underlying io object
        match Pin::new(&mut mut_self.io).poll_read(cx, &mut input_buf) {
            Poll::Pending => {
                // read nothing this time, but fall through and flush out
                // anything we may have buffered last time
            }

            Poll::Ready(Ok(())) => {
                // successfully read bytes into input_buf
            }

            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }
        };

        // if FRAME_LEN_BYTES is 4, each complete packet looks like:
        //
        //    [len|c|.....data.....]
        //    0   4 5              n
        //
        // where n = 5 + len.
        //
        // the data length is first, followed by a continuation byte: 0x01 means
        // continue.
        //
        // the last packet in read_packet_buf may contain a partial packet from
        // the last read - append to it, optionally creating a new packet(s).

        let mut filled: Vec<u8> = input_buf.filled().to_vec();

        while !filled.is_empty() {
            // If read_packet_buf doesn't contain anything, insert a zero-length
            // packet to begin with.
            if mut_self.read_packet_buf.is_empty() {
                mut_self
                    .read_packet_buf
                    .push_back(Vec::with_capacity(MAX_FRAME_LEN));
            }

            let last_packet = mut_self.read_packet_buf.back_mut().unwrap();

            if last_packet.len() >= HEADER_LEN {
                // in here, the packet has the header part of the packet

                // what is the data length?
                let data_len_bytes: [u8; DATA_LEN] = last_packet[0..DATA_LEN]
                    .try_into()
                    .expect("but I just tested the length!");
                let data_len = u16::from_be_bytes(data_len_bytes) as usize;

                match last_packet.len().cmp(&(HEADER_LEN + data_len)) {
                    Ordering::Less => {
                        // the last packet was not a complete packet, so append
                        // received bytes to it
                        let remaining = (HEADER_LEN + data_len) - last_packet.len();

                        if filled.len() >= remaining {
                            // there are more than enough (or equal to) input bytes
                            // to make a complete packet.
                            last_packet.extend_from_slice(&filled[0..remaining]);
                            let (_, remaining_slice) = filled.split_at(remaining);
                            filled = remaining_slice.to_vec();

                            // go around the loop again to deal with the rest of
                            // filled
                        } else {
                            // in here, filled.len() < remaining, meaning the input
                            // buf didn't have enough bytes to fill out a packet.
                            last_packet.extend_from_slice(&filled);
                            filled.clear();
                        }
                    }

                    Ordering::Equal => {
                        // last packet is a complete packet, need to make a new one
                        mut_self
                            .read_packet_buf
                            .push_back(Vec::with_capacity(MAX_FRAME_LEN));

                        // go around the loop again to deal with the rest of filled
                    }

                    Ordering::Greater => {
                        // In here, last_packet.len() > HEADER_LEN + data_len!
                        panic!("packet is too large!");
                    }
                }
            } else {
                // in here, last_packet.len() < HEADER_LEN so the last packet
                // does not have the header
                let bytes_needed_for_header = HEADER_LEN - last_packet.len();
                if filled.len() < bytes_needed_for_header {
                    // didn't get enough bytes to even have a header!
                    // write in what we do have then return Poll::Pending
                    last_packet.extend_from_slice(&filled);
                    filled.clear();
                } else {
                    // write header into packet
                    last_packet.extend_from_slice(&filled[0..bytes_needed_for_header]);
                    let (_, remaining_slice) = filled.split_at(bytes_needed_for_header);
                    filled = remaining_slice.to_vec();

                    // go around the loop again, this time hitting the other if
                    // branch because we have a frame length
                }
            }
        }

        let mut bytes_written = false;

        // now that we have a list of packets (complete and incomplete),
        // transform them and read them into the supplied output buf. stop if
        // we're about to put more into buf than it can hold, and let the caller
        // deal with it.

        // make sure to put any bytes remaining from our last packet first.
        if !mut_self.remaining.is_empty() {
            if buf.remaining() < mut_self.remaining.len() {
                // there isn't enough space in the output buffer for the
                // remaining bytes
                let cutoff = buf.remaining();
                buf.put_slice(&mut_self.remaining[..cutoff]);

                let (_, remaining) = mut_self.remaining.split_at(cutoff);
                let remaining = remaining.to_vec();

                mut_self.remaining.clear();
                mut_self.remaining.extend_from_slice(&remaining);

                // we have to return here, buf is now full.
                return Poll::Ready(Ok(()));
            }

            buf.put_slice(&mut_self.remaining);
            mut_self.remaining.clear();

            bytes_written = true;
        }

        // Transform packet data, and write it to the output buf
        'outer: while !mut_self.read_packet_buf.is_empty() {
            // accumulate packet data, taking into account continuation bytes
            let mut msg: Vec<u8> = Vec::with_capacity(MAX_FRAME_LEN);
            let mut packets = 0;
            let mut saw_all_packets = false;

            for packet in mut_self.read_packet_buf.iter() {
                // check for incomplete packets

                if packet.len() < HEADER_LEN {
                    // not enough packet yet for a frame length
                    break 'outer;
                }

                let data_len_bytes: [u8; DATA_LEN] =
                    packet[0..DATA_LEN].try_into().expect("I just checked len!");
                let data_len: usize = u16::from_be_bytes(data_len_bytes) as usize;

                if packet.len() < (HEADER_LEN + data_len) {
                    // not enough packet yet
                    break 'outer;
                }

                assert_eq!(packet.len(), HEADER_LEN + data_len);

                // here, we have a complete packet. append the data to our buffer.
                msg.extend_from_slice(&packet[HEADER_LEN..]);
                packets += 1;

                // now check to see if the continuation is set. if it is, continue
                // to the next packet. if it's not, break here
                if packet[CONTINUATION_BYTE_OFFSET] == 0x00 {
                    saw_all_packets = true;
                    break;
                }
            }

            if !saw_all_packets {
                break;
            }

            // if it's not, apply the transform to the whole message
            match mut_self.transform.read_payload(&msg, cx) {
                Ok(payload) => {
                    // write payload into output buf
                    if buf.remaining() < payload.len() {
                        // there isn't enough room in buf for all the payload
                        // bytes! write what we can, then store the rest at the
                        // end of `remaining`
                        let cutoff = buf.remaining();

                        buf.put_slice(&payload[..cutoff]);

                        let (_, remaining) = payload.split_at(cutoff);
                        mut_self.remaining.extend_from_slice(remaining);

                        // drop the packets we read, it only contains
                        // untransformed data, and the rest of the transformed
                        // data is in `remaining`
                        mut_self.read_packet_buf.drain(0..packets);

                        // we have to return here, buf is now full.
                        return Poll::Ready(Ok(()));
                    }

                    assert!(buf.remaining() >= payload.len());
                    buf.put_slice(&payload);
                    mut_self.read_packet_buf.drain(0..packets);

                    bytes_written = true;
                }

                Err(e) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e,
                    )));
                }
            }
        }

        if bytes_written {
            Poll::Ready(Ok(()))
        } else {
            if !mut_self.read_packet_buf.is_empty() {
                // notify the executor that this is ready to run, again, because
                // we have buffered packets with untransformed data
                cx.waker().wake_by_ref();
            }

            Poll::Pending
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin, U: PacketDataTransform> AsyncWrite for AsyncIoPacket<T, U> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let mut_self = self.get_mut();

        // transform the whole buf into a message
        let msg = match mut_self.transform.write_message(buf, cx) {
            Ok(msg) => msg,
            Err(e) => {
                return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, e)));
            }
        };

        // take chunks out of buf, make packets out of it. set the continuation
        // byte on each packet except the last one.
        for chunk in msg.chunks(MAX_DATA_LEN) {
            // create a new packet
            let mut packet = vec![0u8; MAX_FRAME_LEN];

            // 1. write out the data length
            let data_len = chunk.len();
            assert!((HEADER_LEN + data_len) <= MAX_FRAME_LEN);
            packet[0..DATA_LEN].copy_from_slice(&u16::to_be_bytes(data_len as u16)[..DATA_LEN]);

            // 2. set the continuation byte on
            packet[CONTINUATION_BYTE_OFFSET] = 0x01;

            // 3. write out packet data
            packet[HEADER_LEN..(HEADER_LEN + data_len)].copy_from_slice(chunk);

            // 4. truncate and add it to the list of packets
            mut_self
                .write_packet_buf
                .push(packet[..(HEADER_LEN + data_len)].to_vec());
        }

        // for the last packet, set the continuation byte to 0x00
        if let Some(packet) = mut_self.write_packet_buf.last_mut() {
            packet[CONTINUATION_BYTE_OFFSET] = 0x00;
        }

        // write out as much packet data as we can
        match mut_self.poll_write_packets_to_io(cx) {
            Poll::Pending => Poll::Pending,

            Poll::Ready(Ok(_)) => {
                // note here that bytes written must be 0 <= n <= buf.len(),
                // **not** the transformed message length. we consumed all of
                // buf to write it into write_packet_buf, so return to caller
                // that we "wrote" all the bytes. really, we buffered them, so
                // the caller will need to flush to ensure that bytes reach
                // their destination.
                Poll::Ready(Ok(buf.len()))
            }

            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut_self = self.get_mut();

        // flush buffered data
        match mut_self.poll_write_packets_to_io(cx) {
            // cannot complete flush right now
            Poll::Pending => {
                return Poll::Pending;
            }

            // all data flushed out?
            Poll::Ready(Ok(_)) => {
                if mut_self.write_packet_buf.is_empty() {
                    // ok, proceed to flushing self.io
                } else {
                    // flush not complete
                    return Poll::Pending;
                }
            }

            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }
        }

        Pin::new(&mut mut_self.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut_self = self.get_mut();

        match Pin::new(&mut mut_self.io).poll_flush(cx) {
            Poll::Pending => {
                // shutdown flush not complete
                return Poll::Pending;
            }

            Poll::Ready(Ok(())) => {
                // flush completed ok
            }

            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }
        }

        Pin::new(&mut mut_self.io).poll_shutdown(cx)
    }
}
