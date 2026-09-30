//! Concrete ownership bridge from HAP pairing into native AP2 media.
//! Keeps the new MSA SOLO engine independent from the legacy sairplay-engine crate.

use crate::hap_pairing::TransientPairingResult;
use crate::native_codec::{ChaChaAudioCipher, CodecError, NativeAlacEncoder};
use crate::native_io::{LocalMediaPorts, NativeMediaIo};
use std::io;
use std::net::IpAddr;

#[derive(Debug)]
pub enum NativeMediaOwnerError {
    Io(io::Error),
    Codec(CodecError),
}

impl From<io::Error> for NativeMediaOwnerError {
    fn from(value: io::Error) -> Self { Self::Io(value) }
}
impl From<CodecError> for NativeMediaOwnerError {
    fn from(value: CodecError) -> Self { Self::Codec(value) }
}

/// Owns the concrete media resources that exist after HAP pairing and before
/// stream SETUP. MSA uses the pairing shared secret directly as the audio key.
pub struct NativeMediaOwner {
    pub io: NativeMediaIo,
    pub encoder: NativeAlacEncoder,
    pub cipher: ChaChaAudioCipher,
    pub audio_secret: [u8; 32],
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
}

impl NativeMediaOwner {
    pub fn prepare(
        bind_ip: IpAddr,
        sample_rate: u32,
        bit_depth: u16,
        channels: u16,
        pairing: &TransientPairingResult,
    ) -> Result<Self, NativeMediaOwnerError> {
        let io = NativeMediaIo::bind(bind_ip)?;
        let encoder = NativeAlacEncoder::open(sample_rate, bit_depth, channels)?;
        let audio_secret = pairing.audio_secret;
        let cipher = ChaChaAudioCipher::new(audio_secret);
        Ok(Self {
            io,
            encoder,
            cipher,
            audio_secret,
            sample_rate,
            bit_depth,
            channels,
        })
    }

    pub fn local_ports(&self) -> Result<LocalMediaPorts, NativeMediaOwnerError> {
        Ok(self.io.local_ports()?)
    }

    pub fn attach_realtime(
        &mut self,
        receiver_ip: IpAddr,
        data_port: u16,
        control_port: u16,
    ) {
        self.io.attach_remote(receiver_ip, data_port, control_port);
    }

    /// Type-103 has a receiver-owned TCP data listener. Its SETUP response may
    /// omit a remote control port; buffered media itself does not use RTX/sync.
    pub fn attach_buffered(
        &mut self,
        receiver_ip: IpAddr,
        data_port: u16,
        control_port: Option<u16>,
    ) -> Result<(), NativeMediaOwnerError> {
        self.io.attach_data_remote(receiver_ip, data_port);
        if let Some(port) = control_port {
            self.io.attach_control_remote(receiver_ip, port);
        }
        self.io.connect_buffered()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hap_pairing::TransientPairingResult;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    fn pairing() -> TransientPairingResult {
        TransientPairingResult {
            peer: SocketAddr::from((Ipv4Addr::LOCALHOST, 7000)),
            write_key: [1; 32],
            read_key: [2; 32],
            audio_secret: [3; 32],
            session_key: [4; 64],
        }
    }

    #[test]
    fn pairing_audio_secret_becomes_media_cipher_key_owner() {
        let p = pairing();
        let owner = NativeMediaOwner::prepare(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            44_100,
            16,
            2,
            &p,
        ).unwrap();
        assert_eq!(owner.audio_secret, [3; 32]);
        let ports = owner.local_ports().unwrap();
        assert_ne!(ports.data, 0);
        assert_ne!(ports.control, 0);
    }

    #[test]
    fn buffered_attach_connects_receiver_owned_tcp_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let p = pairing();
        let mut owner = NativeMediaOwner::prepare(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            44_100,
            16,
            2,
            &p,
        ).unwrap();
        owner.attach_buffered(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port(), None).unwrap();
        let (_sock, _) = listener.accept().unwrap();
    }
}
