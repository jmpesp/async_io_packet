// Copyright 2022 Oxide Computer Company

use super::*;
use rand::Rng;
use rand::SeedableRng;

/// An OTP PacketDataTransform uses a seedable RNG as a One-Time Pad to encrypt
/// bytes
pub struct OtpPacketDataTransform<T: Rng + SeedableRng + Unpin> {
    rng: T,
}

impl<T: Rng + SeedableRng + Unpin> OtpPacketDataTransform<T> {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: T::seed_from_u64(seed),
        }
    }
}

impl<T: Rng + SeedableRng + Unpin> PacketDataTransform for OtpPacketDataTransform<T> {
    fn read_payload(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        let mut payload = vec![0u8; msg.len()];
        for i in 0..msg.len() {
            payload[i] = msg[i] ^ self.rng.gen::<u8>();
        }
        Ok(payload)
    }

    fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut msg = vec![0u8; payload.len()];
        for i in 0..payload.len() {
            msg[i] = payload[i] ^ self.rng.gen::<u8>();
        }
        Ok(msg)
    }
}
