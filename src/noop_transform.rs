// Copyright 2022 Oxide Computer Company

use super::*;

/// A no-op PacketDataTransform does nothing except copy bytes
#[derive(Debug)]
pub struct NoopPacketDataTransform {}

impl PacketDataTransform for NoopPacketDataTransform {
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

    fn max_payload_bytes(&self) -> Option<usize> {
        None
    }

    fn read_payload(&mut self, msg: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        Ok(msg.to_vec())
    }

    fn write_message(&mut self, payload: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        Ok(payload.to_vec())
    }
}
