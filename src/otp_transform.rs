// Copyright 2022 Oxide Computer Company

use super::*;

use std::fmt;
use std::marker::Send;
use std::marker::Sync;

use rand::Rng;
use rand::SeedableRng;

/// An OTP PacketDataTransform uses a seedable RNG as a One-Time Pad to encrypt
/// bytes
pub struct OtpPacketDataTransform<T: Rng + SeedableRng + Unpin + Send + Sync> {
    rng: T,
}

impl<T: Rng + SeedableRng + Unpin + Send + Sync> Debug for OtpPacketDataTransform<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtpPacketDataTransform").finish()
    }
}

impl<T: Rng + SeedableRng + Unpin + Send + Sync> OtpPacketDataTransform<T> {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: T::seed_from_u64(seed),
        }
    }
}

impl<T: Rng + SeedableRng + Unpin + Send + Sync> PacketDataTransform for OtpPacketDataTransform<T> {
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
        let mut payload = vec![0u8; msg.len()];
        for i in 0..msg.len() {
            payload[i] = msg[i] ^ self.rng.gen::<u8>();
        }
        Ok(payload)
    }

    fn write_message(&mut self, payload: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        let mut msg = vec![0u8; payload.len()];
        for i in 0..payload.len() {
            msg[i] = payload[i] ^ self.rng.gen::<u8>();
        }
        Ok(msg)
    }
}
