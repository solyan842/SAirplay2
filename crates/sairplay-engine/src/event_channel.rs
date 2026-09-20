use crate::{HapControlCipher, HapCryptoError, NativeConnectError, NativeConnectFlow};
use hkdf::Hkdf;
use sha2::Sha512;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

const EVENTS_SALT: &[u8] = b"Events-Salt";
const EVENTS_WRITE_INFO: &[u8] = b"Events-Write-Encryption-Key";
const EVENTS_READ_INFO: &[u8] = b"Events-Read-Encryption-Key";

#[derive(Debug)]
pub enum EventChannelError {
    Flow(NativeConnectError),
    Connect(std::io::Error),
    Configure(std::io::Error),
    Hkdf,
    Crypto(HapCryptoError),
    Read(std::io::Error),
    Write(std::io::Error),
    Closed,
}

impl From<NativeConnectError> for EventChannelError {
    fn from(value: NativeConnectError) -> Self { Self::Flow(value) }
}
impl From<HapCryptoError> for EventChannelError {
    fn from(value: HapCryptoError) -> Self { Self::Crypto(value) }
}

pub fn derive_event_keys(shared_secret: &[u8; 32]) -> Result<([u8; 32], [u8; 32]), EventChannelError> {
    let hk = Hkdf::<Sha512>::new(Some(EVENTS_SALT), shared_secret);

    // Receiver is the logical writer on the reverse event channel:
    // our outbound uses Events-Read, our inbound uses Events-Write.
    let mut out_key = [0u8; 32];
    let mut in_key = [0u8; 32];

    hk.expand(EVENTS_READ_INFO, &mut out_key)
        .map_err(|_| EventChannelError::Hkdf)?;
    hk.expand(EVENTS_WRITE_INFO, &mut in_key)
        .map_err(|_| EventChannelError::Hkdf)?;

    Ok((out_key, in_key))
}

pub struct EventChannel {
    stream: TcpStream,
    cipher: HapControlCipher,
    encrypted_carry: Vec<u8>,
}

impl EventChannel {
    pub fn connect(
        host: IpAddr,
        port: u16,
        shared_secret: &[u8; 32],
        timeout: Duration,
    ) -> Result<Self, EventChannelError> {
        let addr = SocketAddr::new(host, port);
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .map_err(EventChannelError::Connect)?;
        stream.set_nodelay(true).map_err(EventChannelError::Configure)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(EventChannelError::Configure)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(EventChannelError::Configure)?;

        let (out_key, in_key) = derive_event_keys(shared_secret)?;

        Ok(Self {
            stream,
            cipher: HapControlCipher::new(out_key, in_key),
            encrypted_carry: Vec::new(),
        })
    }

    pub fn read_plaintext(&mut self) -> Result<Option<Vec<u8>>, EventChannelError> {
        let mut buf = [0u8; 4096];

        match self.stream.read(&mut buf) {
            Ok(0) => return Err(EventChannelError::Closed),
            Ok(n) => self.encrypted_carry.extend_from_slice(&buf[..n]),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(None);
            }
            Err(err) => return Err(EventChannelError::Read(err)),
        }

        if self.encrypted_carry.len() < 2 {
            return Ok(None);
        }

        let plain_len =
            u16::from_le_bytes([self.encrypted_carry[0], self.encrypted_carry[1]]) as usize;
        if plain_len > 1024 {
            return Err(EventChannelError::Crypto(HapCryptoError::InvalidFrame));
        }

        let frame_len = 2 + plain_len + 16;
        if self.encrypted_carry.len() < frame_len {
            return Ok(None);
        }

        let frame = self.encrypted_carry[..frame_len].to_vec();
        let plain = self.cipher.decrypt(&frame)?;
        self.encrypted_carry.drain(..frame_len);
        Ok(Some(plain))
    }

    pub fn write_plaintext(&mut self, plaintext: &[u8]) -> Result<(), EventChannelError> {
        let wire = self.cipher.encrypt(plaintext)?;
        self.stream.write_all(&wire).map_err(EventChannelError::Write)
    }

    pub fn read_counter(&self) -> u64 { self.cipher.read_counter() }
    pub fn write_counter(&self) -> u64 { self.cipher.write_counter() }
}

pub fn open_event_channel(
    flow: &mut NativeConnectFlow,
    host: IpAddr,
    event_port: u16,
    shared_secret: &[u8; 32],
    timeout: Duration,
) -> Result<EventChannel, EventChannelError> {
    if flow.phase() != crate::NativePhase::SessionSetup {
        flow.event_channel_open()?;
        unreachable!("event_channel_open succeeds only from SessionSetup");
    }

    let channel = EventChannel::connect(host, event_port, shared_secret, timeout)?;

    // Advance only once TCP connect and event-key derivation succeeded.
    flow.event_channel_open()?;
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativePhase;
    use std::net::{Ipv4Addr, TcpListener};
    use std::thread;

    fn session_setup_flow() -> NativeConnectFlow {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow.session_setup().unwrap();
        flow
    }

    #[test]
    fn event_keys_are_directionally_distinct_and_deterministic() {
        let secret = [0x33u8; 32];
        let (out1, in1) = derive_event_keys(&secret).unwrap();
        let (out2, in2) = derive_event_keys(&secret).unwrap();

        assert_eq!(out1, out2);
        assert_eq!(in1, in2);
        assert_ne!(out1, in1);
    }

    #[test]
    fn event_connect_advances_flow_only_after_tcp_is_open() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
        });

        let mut flow = session_setup_flow();
        let secret = [0x44u8; 32];
        let _channel = open_event_channel(
            &mut flow,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            addr.port(),
            &secret,
            Duration::from_secs(1),
        ).unwrap();

        assert_eq!(flow.phase(), NativePhase::EventChannelOpen);
        server.join().unwrap();
    }

    #[test]
    fn failed_event_connect_keeps_flow_at_session_setup() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut flow = session_setup_flow();
        let secret = [0x45u8; 32];
        let result = open_event_channel(
            &mut flow,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            &secret,
            Duration::from_millis(200),
        );

        assert!(matches!(result, Err(EventChannelError::Connect(_))));
        assert_eq!(flow.phase(), NativePhase::SessionSetup);
    }

    #[test]
    fn event_channel_uses_independent_hap_counters_and_swapped_keys() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let secret = [0x55u8; 32];
        let (sender_out, sender_in) = derive_event_keys(&secret).unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();

            // Receiver perspective is opposite to sender perspective.
            let mut receiver_cipher = HapControlCipher::new(sender_in, sender_out);

            let command = b"POST /command RTSP/1.0\r\nCSeq: 9\r\nContent-Length: 0\r\n\r\n";
            let wire = receiver_cipher.encrypt(command).unwrap();
            socket.write_all(&wire).unwrap();

            let mut carry = Vec::new();
            let mut buf = [0u8; 4096];
            let reply = loop {
                let n = socket.read(&mut buf).unwrap();
                carry.extend_from_slice(&buf[..n]);
                if carry.len() < 2 { continue; }
                let plen = u16::from_le_bytes([carry[0], carry[1]]) as usize;
                let frame_len = 2 + plen + 16;
                if carry.len() >= frame_len {
                    break receiver_cipher.decrypt(&carry[..frame_len]).unwrap();
                }
            };

            let text = String::from_utf8(reply).unwrap();
            assert!(text.starts_with("RTSP/1.0 200 OK\r\n"));
            assert!(text.contains("CSeq: 9\r\n"));
        });

        let mut channel = EventChannel::connect(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            addr.port(),
            &secret,
            Duration::from_secs(1),
        ).unwrap();

        let command = loop {
            if let Some(plain) = channel.read_plaintext().unwrap() {
                break plain;
            }
        };
        let text = String::from_utf8(command).unwrap();
        assert!(text.starts_with("POST /command RTSP/1.0\r\n"));

        channel.write_plaintext(
            b"RTSP/1.0 200 OK\r\nContent-Length: 0\r\nAudio-Latency: 0\r\nCSeq: 9\r\n\r\n"
        ).unwrap();

        assert_eq!(channel.read_counter(), 1);
        assert_eq!(channel.write_counter(), 1);

        server.join().unwrap();
    }
}
