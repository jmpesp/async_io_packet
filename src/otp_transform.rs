// Copyright 2022 Oxide Computer Company

use super::*;

use std::fmt;
use std::marker::Send;
use std::marker::Sync;

use rand::Rng;
use rand::SeedableRng;

#[allow(clippy::large_enum_variant)]
enum State {
    Invalid,

    HandshakingSend,
    HandshakingWait,

    Transport,
}

/// An OTP PacketDataTransform uses a seedable RNG as a One-Time Pad to encrypt
/// bytes. Optionally, it can have a stupidly large handshake.
pub struct OtpPacketDataTransform<T: Rng + SeedableRng + Unpin + Send + Sync> {
    rng: T,
    state: State,
    handshake_bytes: Option<Vec<u8>>,
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
            state: State::Transport,
            handshake_bytes: None,
        }
    }

    pub fn client(seed: u64) -> Self {
        Self {
            rng: T::seed_from_u64(seed),
            state: State::HandshakingSend,
            handshake_bytes: None,
        }
    }

    pub fn server(seed: u64) -> Self {
        Self {
            rng: T::seed_from_u64(seed),
            state: State::HandshakingWait,
            handshake_bytes: None,
        }
    }
}

impl<T: Rng + SeedableRng + Unpin + Send + Sync> PacketDataTransform for OtpPacketDataTransform<T> {
    fn handshaking(&self) -> bool {
        matches!(self.state, State::HandshakingSend | State::HandshakingWait)
    }

    fn need_to_start_handshake(&mut self) -> bool {
        matches!(self.state, State::HandshakingSend)
    }

    fn handshake_step(&mut self, msg: &[u8]) -> Result<()> {
        // if this is in wait, but the message is blank, then skip
        if matches!(self.state, State::HandshakingWait) && msg.is_empty() {
            // this side doesn't kick off handshake
            return Ok(());
        }

        let old_state = std::mem::replace(&mut self.state, State::Invalid);
        self.state = match old_state {
            State::Invalid => {
                panic!("saw transient invalid state!");
            }

            State::HandshakingSend => {
                let len: usize = 100000;

                let mut buf = vec![0u8; len];
                self.rng.fill(&mut buf[..]);

                self.handshake_bytes = Some(buf);

                State::Transport
            }

            State::HandshakingWait => {
                let len: usize = 100000;
                if len != msg.len() {
                    bail!("otp handshake length invalid!");
                }

                let mut buf = vec![0u8; len];
                self.rng.fill(&mut buf[..]);

                if buf != msg {
                    bail!("otp handshake bytes invalid!");
                }

                State::Transport
            }

            State::Transport => {
                bail!("completed handshake already!");
            }
        };

        Ok(())
    }

    fn handshake_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        let bytes = self.handshake_bytes.take();
        Ok(bytes)
    }

    fn max_payload_bytes(&self) -> Option<usize> {
        None
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
