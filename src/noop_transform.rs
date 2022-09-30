// Copyright 2022 Oxide Computer Company

use super::*;

/// A no-op PacketDataTransform does nothing except copy bytes
pub struct NoopPacketDataTransform {}

impl PacketDataTransform for NoopPacketDataTransform {
    fn read_payload(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        Ok(msg.to_vec())
    }

    fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        Ok(payload.to_vec())
    }
}
