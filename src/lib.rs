// Copyright 2022 Oxide Computer Company

use core::pin::Pin;
use core::task::Context;
use core::task::Poll;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::fmt::Debug;
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

mod noise_transform;
pub use noise_transform::*;

pub trait PacketDataTransform: Unpin + Debug + Send + Sync {
    /// If this implementation of PacketDataTransform needs to perform some sort
    /// of handshake, return true while it is handshaking.
    fn handshaking(&self) -> bool;

    /// Does this implementation need to start handshaking?
    fn need_to_start_handshake(&mut self) -> bool;

    /// Drive the transform's handshake by passing in any framed handshake bytes
    /// received.
    fn handshake_step(&mut self, msg: &[u8]) -> Result<()>;

    /// Check if there are additional handshake bytes to send, and consume them
    /// if so.
    fn handshake_bytes(&mut self) -> Result<Option<Vec<u8>>>;

    /// Read received packeted data, optionally apply a transform, and return
    /// a payload buffer.
    fn read_payload(&mut self, msg: &[u8], cx: &mut Context<'_>) -> Result<Vec<u8>>;

    /// Read a payload, optionally apply a transform, and return a message that
    /// will eventually be sent as packet data.
    fn write_message(&mut self, payload: &[u8], cx: &mut Context<'_>) -> Result<Vec<u8>>;
}

// Blanket implementation for Boxed T
impl PacketDataTransform for Box<dyn PacketDataTransform> {
    fn handshaking(&self) -> bool {
        PacketDataTransform::handshaking(self.as_ref())
    }

    fn need_to_start_handshake(&mut self) -> bool {
        PacketDataTransform::need_to_start_handshake(self.as_mut())
    }

    fn handshake_step(&mut self, msg: &[u8]) -> Result<()> {
        PacketDataTransform::handshake_step(self.as_mut(), msg)
    }

    fn handshake_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        PacketDataTransform::handshake_bytes(self.as_mut())
    }

    fn read_payload(&mut self, msg: &[u8], cx: &mut Context<'_>) -> Result<Vec<u8>> {
        PacketDataTransform::read_payload(self.as_mut(), msg, cx)
    }

    fn write_message(&mut self, payload: &[u8], cx: &mut Context<'_>) -> Result<Vec<u8>> {
        PacketDataTransform::write_message(self.as_mut(), payload, cx)
    }
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

    /// remaining transformed bytes that need to be read during poll_read
    remaining: Vec<u8>,

    // for writes
    write_packet_buf: Vec<Vec<u8>>,
}

impl<T: AsyncRead + AsyncWrite + Unpin, U: PacketDataTransform> Unpin for AsyncIoPacket<T, U> {}

// Each packet has a maximum size of 65536 bytes.
const MAX_FRAME_LEN: usize = u16::MAX as usize;

// The first bytes store the data length
const DATA_LEN: usize = 2;

// The header is the data length, plus a metadata byte
const HEADER_LEN: usize = DATA_LEN + 1;
const METADATA_BYTE_OFFSET: usize = DATA_LEN;

const CONTINUATION_FLAG: u8 = 0x01;
const HANDSHAKE_FLAG: u8 = 0x02;

fn continuation_packet(packet: &[u8]) -> bool {
    (packet[METADATA_BYTE_OFFSET] & CONTINUATION_FLAG) == CONTINUATION_FLAG
}

fn handshake_packet(packet: &[u8]) -> bool {
    (packet[METADATA_BYTE_OFFSET] & HANDSHAKE_FLAG) == HANDSHAKE_FLAG
}

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

    /// Take chunks out of data and make write packets out of it. set the
    /// continuation byte on each packet except the last one. Optionally flag
    /// these packets as handshake packets.
    fn make_write_packets_from_data(&mut self, data: Vec<u8>, handshake: bool) {
        for chunk in data.chunks(MAX_DATA_LEN) {
            // create a new packet
            let mut packet = vec![0u8; MAX_FRAME_LEN];

            // 1. write out the data length
            let data_len = chunk.len();
            assert!((HEADER_LEN + data_len) <= MAX_FRAME_LEN);
            packet[0..DATA_LEN].copy_from_slice(&u16::to_be_bytes(data_len as u16)[..DATA_LEN]);

            // 2a. set the continuation flag on
            packet[METADATA_BYTE_OFFSET] |= CONTINUATION_FLAG;

            // 2b. optionally set the handshake flag
            if handshake {
                packet[METADATA_BYTE_OFFSET] |= HANDSHAKE_FLAG;
            }

            // 3. write out packet data
            packet[HEADER_LEN..(HEADER_LEN + data_len)].copy_from_slice(chunk);

            // 4. truncate and add it to the list of packets
            self.write_packet_buf
                .push(packet[..(HEADER_LEN + data_len)].to_vec());
        }

        // for the last packet, set the continuation flag off
        if let Some(packet) = self.write_packet_buf.last_mut() {
            packet[METADATA_BYTE_OFFSET] &= !CONTINUATION_FLAG;
        }
    }

    /// Write out as much of what is in `write_packet_buf` as possible.
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
                // we have either handshaking to do or buffered packets
                if self.transform.handshaking() || !self.write_packet_buf.is_empty() {
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

        // Before transforming any packet data, check to see if we need to
        // kick off handshaking. Transforming packet data cannot happen
        // until the transform's handshake is complete.
        if mut_self.transform.handshaking() && mut_self.transform.need_to_start_handshake() {
            // There's no message yet to receive, send &[]
            mut_self
                .transform
                .handshake_step(&[])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

            // If there are handshake bytes to send to the other side, send them
            if let Some(bytes) = mut_self
                .transform
                .handshake_bytes()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
            {
                // create a new handshake packet(s)
                mut_self.make_write_packets_from_data(bytes, true);

                // send it out!
                match mut_self.poll_write_packets_to_io(cx) {
                    Poll::Pending => {}

                    // If this is incomplete, there will be packets left in
                    // write_packet_buf. those will get sent out as part of the
                    // next block of code.
                    Poll::Ready(Ok(_n)) => {}

                    Poll::Ready(Err(e)) => {
                        // error writing out handshake bytes
                        return Poll::Ready(Err(e));
                    }
                }

                // we have to return Poll::Pending from this poll_read,
                // because we've only read handshake bytes, not actual
                // bytes, and we haven't written anything into buf. notify
                // the executor that we need to wake up to continue the
                // handshake.
                cx.waker().wake_by_ref();
                return Poll::Pending;
            } else {
                // no handshake bytes to send. this usually means the
                // handshake is done. notify the executor that we need to
                // wake up to read actual bytes, and return Poll::Pending.
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }

        // If there's still handshake packets to go out, send them out
        if mut_self.transform.handshaking() {
            while !mut_self.write_packet_buf.is_empty() {
                match mut_self.poll_write_packets_to_io(cx) {
                    Poll::Pending => {
                        // We wrote out as much as we could. Return Pending, but
                        // we still need to be woken up to continue the
                        // handshake.
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }

                    Poll::Ready(Ok(_n)) => {
                        // continue looping to write all the handshake packets
                        // out
                    }

                    Poll::Ready(Err(e)) => {
                        // error writing out handshake bytes
                        return Poll::Ready(Err(e));
                    }
                }
            }
        }

        // Fill input buf by polling the underlying io object, then turn those
        // into packets.

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
        //    [len|m|.....data.....]
        //    0   4 5              n
        //
        // where n = 5 + len.
        //
        // the data length is first, followed by a metadata byte:
        //
        // - 0x01 is the continuation flag
        // - 0x02 is the handshake flag
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

        // Pass packeted data to the transform. Optionally, write to the output
        // buf.
        'outer: while !mut_self.read_packet_buf.is_empty() {
            // accumulate packet data, taking into account continuation bytes
            let mut msg: Vec<u8> = Vec::with_capacity(MAX_FRAME_LEN);
            let mut packets = 0;
            let mut saw_all_packets = false;
            let mut handshaking_flag: Vec<bool> = vec![];

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

                // record if this packet had handshaking flag
                handshaking_flag.push(handshake_packet(packet));

                // now check to see if the continuation is set. if it is, continue
                // to the next packet. if it's not, break here.
                if !continuation_packet(packet) {
                    saw_all_packets = true;
                    break;
                }
            }

            // Bail early if we haven't seen a non-continuation packet
            if !saw_all_packets {
                break;
            }

            // Before transforming any packet data, check to see if we're
            // handshaking still. Transforming packet data cannot happen until
            // the transform's handshake is complete.
            if mut_self.transform.handshaking() {
                // If we're still handshaking, expect that each packet we've
                // seen is a handshake packet, and that the other side of this
                // communication is buffering actual data.
                if !handshaking_flag.iter().all(|x| *x) {
                    // if not, this is a bug.
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "During handshaking, not all packets were handshake packets!",
                    )));
                }

                // Pass this to the transform as a handshake response
                mut_self.transform.handshake_step(&msg).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;

                // Drop the packets we read, we consumed the handshake bytes
                mut_self.read_packet_buf.drain(0..packets);

                if let Some(bytes) = mut_self.transform.handshake_bytes().map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })? {
                    // create a new handshake packet(s)
                    mut_self.make_write_packets_from_data(bytes, true);

                    // send it out!
                    match mut_self.poll_write_packets_to_io(cx) {
                        Poll::Pending => {}

                        Poll::Ready(Ok(_n)) => {
                            // If this is incomplete, the next round will send
                            // it out.
                        }

                        Poll::Ready(Err(e)) => {
                            // error writing out handshake bytes
                            return Poll::Ready(Err(e));
                        }
                    }

                    // we have to return Poll::Pending from this poll_read,
                    // because we've only read and written handshake bytes, not
                    // actual bytes, and we haven't written anything into buf.
                    // notify the executor that we need to wake up to continue
                    // the handshake.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                } else {
                    // no handshake bytes to send. this usually means the
                    // handshake is done. notify the executor that we need to
                    // wake up to read actual bytes, and return Poll::Pending.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            } else {
                // No longer handshaking - apply the transform to the whole message
                if handshaking_flag.iter().any(|x| *x) {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Not handshaking but saw handshake packets!",
                    )));
                }

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
        }

        if bytes_written {
            Poll::Ready(Ok(()))
        } else {
            if mut_self.transform.handshaking() || !mut_self.read_packet_buf.is_empty() {
                // notify the executor that this is ready to run, again, because
                // we have either handshaking to do or buffered packets with
                // untransformed data
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
        let mut mut_self = self.get_mut();

        if mut_self.transform.handshaking() {
            // If we're still handshaking, we can't transform any bytes yet.
            // Either kick off or continue the handshake. `poll_read` drives the
            // handshake, so call it here.
            let mut bytes = vec![0u8; 65536];
            let mut rb = tokio::io::ReadBuf::new(&mut bytes);
            let result = Pin::new(&mut mut_self).poll_read(cx, &mut rb);

            // `poll_read` will not return any data while handshaking
            assert!(rb.filled().is_empty());

            return match result {
                Poll::Pending => {
                    // need to wake up to continue handshake
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }

                Poll::Ready(Ok(())) => {
                    // We're still handshaking, or we're ready to actually
                    // write. Either way, return Pending,
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }

                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            };
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // transform the whole buf into a message
        let msg = match mut_self.transform.write_message(buf, cx) {
            Ok(msg) => msg,
            Err(e) => {
                return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, e)));
            }
        };

        mut_self.make_write_packets_from_data(msg, false);

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
