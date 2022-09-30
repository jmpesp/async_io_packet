// Copyright 2022 Oxide Computer Company

use super::*;

// from https://en.wikipedia.org/wiki/Fletcher%27s_checksum
fn fletcher32(data: &[u8]) -> u32 {
    let mut sum1: u32 = 0;
    let mut sum2: u32 = 0;

    for chunk in data.chunks(2) {
        let i = if chunk.len() == 2 {
            u16::from_le_bytes(chunk[0..2].try_into().expect("just checked the size!"))
        } else {
            u16::from_le_bytes([chunk[0], 0])
        };
        sum1 = (sum1 + i as u32) % 0xFFFF;
        sum2 = (sum2 + sum1) % 0xFFFF;
    }

    (sum2 << 16) | sum1
}

#[test]
fn test_fletcher32() {
    assert_eq!(fletcher32("abcde".as_bytes()), 4031760169);
    assert_eq!(fletcher32("abcdef".as_bytes()), 1448095018);
    assert_eq!(fletcher32("abcdefgh".as_bytes()), 3957429649);
}

// Prepend a Fletcher32 checksum to the beginning. Note: this is an example of a
// transform that increases the size of the data, requiring continuation
// packets.
#[derive(Debug)]
pub struct ChecksumPacketDataTransform {}

impl ChecksumPacketDataTransform {
    fn read_payload(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        if msg.len() < 4 {
            bail!("no prepended checksum!");
        }

        let computed_checksum: u32 = fletcher32(&msg[4..]);
        let msg_checksum = u32::from_le_bytes(msg[0..4].try_into().expect("just checked length!"));

        if computed_checksum != msg_checksum {
            bail!(
                "checksum mismatch! {:0X} != {:0X}",
                computed_checksum,
                msg_checksum
            );
        }

        Ok(msg[4..].to_vec())
    }

    fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut msg = vec![0u8; payload.len() + 4];

        let computed_checksum: u32 = fletcher32(payload);

        msg[0..4].copy_from_slice(&u32::to_le_bytes(computed_checksum));
        msg[4..].copy_from_slice(payload);

        Ok(msg)
    }
}

impl PacketDataTransform for ChecksumPacketDataTransform {
    fn handshaking(&self) -> bool {
        false
    }

    fn need_to_start_handshake(&mut self) -> bool {
        false
    }

    fn handshake_step(&mut self, _msg: &[u8]) -> Result<()> {
        Ok(())
    }

    fn handshake_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn read_payload(&mut self, msg: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        self.read_payload(msg)
    }

    fn write_message(&mut self, payload: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        self.write_message(payload)
    }
}

#[test]
fn test_round_trip() {
    let mut chksum = ChecksumPacketDataTransform {};

    let data: &[u8] = "test post please ignore".as_bytes();

    let message = chksum.write_message(data).unwrap();

    assert_eq!(&chksum.read_payload(&message).unwrap(), data);
}
