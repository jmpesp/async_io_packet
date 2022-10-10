// Copyright 2022 Oxide Computer Company

use super::*;

use std::fmt;

// Use the Noise protocol to encrypt communications

#[derive(Debug)]
pub enum NoiseProtocolStep {
    ServerStart,
    ServerWait,

    ClientStart,
    ClientWait,
}

#[allow(clippy::large_enum_variant)]
pub enum NoiseState {
    Invalid,

    Handshake {
        state: NoiseProtocolStep,
        noise: snow::HandshakeState,
        handshake_bytes: Option<Vec<u8>>,
    },

    Transport {
        noise: snow::TransportState,
        final_handshake_bytes: Option<Vec<u8>>,
    },
}

impl Debug for NoiseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoiseState::Invalid => {
                panic!("invalid state seen!");
            }

            NoiseState::Handshake {
                state,
                noise: _,
                handshake_bytes,
            } => f
                .debug_struct("NoiseState::Handshake")
                .field("state", &state)
                .field("handshake_bytes", &handshake_bytes)
                .finish(),

            NoiseState::Transport {
                noise: _,
                final_handshake_bytes,
            } => f
                .debug_struct("NoiseState::Transport")
                .field("final_handshake_bytes", &final_handshake_bytes)
                .finish(),
        }
    }
}

pub struct NoisePacketDataTransform {
    noise_state: NoiseState,
}

impl Debug for NoisePacketDataTransform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoisePacketDataTransform")
            .field("noise_state", &self.noise_state)
            .finish()
    }
}

impl NoisePacketDataTransform {
    pub fn server(psk: &[u8]) -> Result<Self> {
        let builder: snow::Builder<'_> =
            snow::Builder::new("Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s".parse()?);
        let static_key = builder.generate_keypair()?.private;
        let noise = builder
            .local_private_key(&static_key)
            .psk(3, psk)
            .build_responder()?;

        Ok(Self {
            noise_state: NoiseState::Handshake {
                state: NoiseProtocolStep::ServerStart,
                noise,
                handshake_bytes: None,
            },
        })
    }

    pub fn client(psk: &[u8]) -> Result<Self> {
        let builder: snow::Builder<'_> =
            snow::Builder::new("Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s".parse()?);
        let static_key = builder.generate_keypair()?.private;
        let noise = builder
            .local_private_key(&static_key)
            .psk(3, psk)
            .build_initiator()?;

        Ok(Self {
            noise_state: NoiseState::Handshake {
                state: NoiseProtocolStep::ClientStart,
                noise,
                handshake_bytes: None,
            },
        })
    }
}

impl PacketDataTransform for NoisePacketDataTransform {
    fn handshaking(&self) -> bool {
        matches!(
            self.noise_state,
            NoiseState::Handshake {
                state: _,
                noise: _,
                handshake_bytes: _
            }
        )
    }

    fn need_to_start_handshake(&mut self) -> bool {
        matches!(
            self.noise_state,
            NoiseState::Handshake {
                state: NoiseProtocolStep::ClientStart,
                noise: _,
                handshake_bytes: _
            }
        )
    }

    fn handshake_step(&mut self, msg: &[u8]) -> Result<()> {
        // if this is in server wait, but the message is blank, then skip
        if matches!(
            self.noise_state,
            NoiseState::Handshake {
                state: NoiseProtocolStep::ServerStart,
                ..
            }
        ) && msg.is_empty()
        {
            // server doesn't kick off handshake
            return Ok(());
        }

        let old_noise_state = std::mem::replace(&mut self.noise_state, NoiseState::Invalid);
        self.noise_state = match old_noise_state {
            NoiseState::Invalid => {
                panic!("saw transient invalid state!");
            }

            NoiseState::Handshake {
                state,
                mut noise,
                handshake_bytes: _,
            } => {
                match state {
                    NoiseProtocolStep::ClientStart => {
                        // -> e
                        let mut buf = vec![0u8; 65535];
                        let len = noise.write_message(&[], &mut buf)?;
                        buf.truncate(len);

                        // next, wait for e, ee, s, es from server
                        NoiseState::Handshake {
                            state: NoiseProtocolStep::ClientWait,
                            noise,
                            handshake_bytes: Some(buf),
                        }
                    }

                    NoiseProtocolStep::ServerStart => {
                        let mut buf = vec![0u8; 65535];

                        // <- e
                        noise.read_message(msg, &mut buf)?;

                        // -> e, ee, s, es
                        let len = noise.write_message(&[0u8; 0], &mut buf)?;
                        buf.truncate(len);

                        // next, wait for s, se from client
                        NoiseState::Handshake {
                            state: NoiseProtocolStep::ServerWait,
                            noise,
                            handshake_bytes: Some(buf),
                        }
                    }

                    NoiseProtocolStep::ClientWait => {
                        let mut buf = vec![0u8; 65535];

                        // <- e, ee, s, es
                        noise.read_message(msg, &mut buf)?;

                        // -> s, se
                        let len = noise.write_message(&[], &mut buf)?;
                        buf.truncate(len);

                        // Transition the state machine into transport mode now that the
                        // handshake is complete.
                        NoiseState::Transport {
                            noise: noise.into_transport_mode()?,
                            final_handshake_bytes: Some(buf),
                        }
                    }

                    NoiseProtocolStep::ServerWait => {
                        let mut buf = vec![0u8; 65535];

                        // <- s, se
                        noise.read_message(msg, &mut buf)?;

                        // Transition the state machine into transport mode now that the
                        // handshake is complete.
                        NoiseState::Transport {
                            noise: noise.into_transport_mode()?,
                            final_handshake_bytes: None,
                        }
                    }
                }
            }

            NoiseState::Transport {
                noise: _,
                final_handshake_bytes: _,
            } => {
                bail!("completed handshake already!");
            }
        };

        Ok(())
    }

    fn handshake_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        match &mut self.noise_state {
            NoiseState::Invalid => {
                panic!("saw transient invalid state!");
            }

            NoiseState::Handshake {
                state: _,
                noise: _,
                handshake_bytes,
            } => Ok(handshake_bytes.take()),

            NoiseState::Transport {
                noise: _,
                final_handshake_bytes,
            } => Ok(final_handshake_bytes.take()),
        }
    }

    fn max_payload_bytes(&self) -> Option<usize> {
        // In the snow crate's src/constants.rs:
        //
        //     TAG_LEN = 16
        //     MAXMSGLEN = 65535
        //
        // truncate the maximum payload that this transform can do based on
        // these
        Some(65535 - 16)
    }

    fn read_payload(&mut self, msg: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        match &mut self.noise_state {
            NoiseState::Invalid => {
                panic!("saw transient invalid state!");
            }

            NoiseState::Handshake {
                state: _,
                noise: _,
                handshake_bytes: _,
            } => {
                bail!("need to complete handshake first!");
            }

            NoiseState::Transport {
                noise,
                final_handshake_bytes: _,
            } => {
                let mut payload = vec![0u8; 65535]; // max of Noise protocol
                let n = noise.read_message(msg, &mut payload)?;
                payload.truncate(n);
                Ok(payload)
            }
        }
    }

    fn write_message(&mut self, payload: &[u8], _cx: &mut Context<'_>) -> Result<Vec<u8>> {
        match &mut self.noise_state {
            NoiseState::Invalid => {
                panic!("saw transient invalid state!");
            }

            NoiseState::Handshake {
                state: _,
                noise: _,
                handshake_bytes: _,
            } => {
                bail!("need to complete handshake first!");
            }

            NoiseState::Transport {
                noise,
                final_handshake_bytes: _,
            } => {
                let mut msg = vec![0u8; 65535]; // max of Noise protocol
                let n = noise.write_message(payload, &mut msg)?;
                msg.truncate(n);
                Ok(msg)
            }
        }
    }
}
