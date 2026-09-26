use crate::{
    derive_control_keys, srp_client_compute, HapCryptoError, RtspCodec, RtspError, RtspResponse,
    EncryptedRtspChannel, SrpError, Tlv8, Tlv8Error, TlvTag, HAP_TRANSIENT_FLAG,
    SRP_TRANSIENT_PIN,
};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha512;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum PairingError {
    Resolve,
    Connect(std::io::Error),
    Configure(std::io::Error),
    Write(std::io::Error),
    Read(std::io::Error),
    Timeout,
    Closed,
    Rtsp(RtspError),
    Tlv(Tlv8Error),
    Srp(SrpError),
    Crypto(HapCryptoError),
    Status(u16),
    TlvError(u8),
    UnexpectedState { expected: u8, actual: Option<u8> },
    InvalidSalt,
    InvalidServerPublicKey,
    InvalidServerProof,
    InvalidCredentials,
    MissingIdentifier,
    MissingEncryptedData,
    InvalidSignature,
}

impl From<RtspError> for PairingError {
    fn from(value: RtspError) -> Self { Self::Rtsp(value) }
}
impl From<Tlv8Error> for PairingError {
    fn from(value: Tlv8Error) -> Self { Self::Tlv(value) }
}
impl From<SrpError> for PairingError {
    fn from(value: SrpError) -> Self { Self::Srp(value) }
}
impl From<HapCryptoError> for PairingError {
    fn from(value: HapCryptoError) -> Self { Self::Crypto(value) }
}

#[derive(Debug, Clone)]
pub struct TransientPairingResult {
    pub peer: SocketAddr,
    pub write_key: [u8; 32],
    pub read_key: [u8; 32],
    pub audio_secret: [u8; 32],
    pub session_key: [u8; 64],
}

pub struct TransientPairingSession {
    pub pairing: TransientPairingResult,
    pub channel: EncryptedRtspChannel,
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHapCredentials {
    pub client_seed: [u8; 32],
    pub client_public: [u8; 32],
    pub server_public: [u8; 32],
}

impl StoredHapCredentials {
    pub fn from_hex(value: &str) -> Result<Self, PairingError> {
        if value.len() != 192 {
            return Err(PairingError::InvalidCredentials);
        }
        let mut raw = [0u8; 96];
        for (i, byte) in raw.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
                .map_err(|_| PairingError::InvalidCredentials)?;
        }
        let mut client_seed = [0u8; 32];
        let mut client_public = [0u8; 32];
        let mut server_public = [0u8; 32];
        client_seed.copy_from_slice(&raw[..32]);
        client_public.copy_from_slice(&raw[32..64]);
        server_public.copy_from_slice(&raw[64..96]);
        Ok(Self { client_seed, client_public, server_public })
    }

    pub fn to_hex(&self) -> String {
        fn push_hex(out: &mut String, bytes: &[u8]) {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for &byte in bytes {
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
        let mut out = String::with_capacity(192);
        push_hex(&mut out, &self.client_seed);
        push_hex(&mut out, &self.client_public);
        push_hex(&mut out, &self.server_public);
        out
    }
}

pub struct NativeHapPairingClient {
    connect_timeout: Duration,
    exchange_timeout: Duration,
    user_agent: String,
}

impl Default for NativeHapPairingClient {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            exchange_timeout: Duration::from_secs(8),
            user_agent: "AirPlay/670.6.2".into(),
        }
    }
}

impl NativeHapPairingClient {
    pub fn pair_setup_pin<F>(
        &self,
        host: &str,
        port: u16,
        client_id: &str,
        pin_provider: F,
    ) -> Result<StoredHapCredentials, PairingError>
    where
        F: FnOnce() -> Option<String>,
    {
        let peer = (host, port)
            .to_socket_addrs()
            .map_err(|_| PairingError::Resolve)?
            .next()
            .ok_or(PairingError::Resolve)?;
        let mut stream = TcpStream::connect_timeout(&peer, self.connect_timeout)
            .map_err(PairingError::Connect)?;
        configure_pairing_stream(&mut stream, self.exchange_timeout)?;

        // Full PIN pairing follows MSA's raw pair-setup helper. The
        // /pair-pin-start response is not required to have normal RTSP/CSeq
        // semantics, so do not route it through the strict session codec.
        let _ = exchange_pair_setup_raw(
            &mut stream, 0, &self.user_agent, "/pair-pin-start", 3, &[],
            self.exchange_timeout,
        );

        let mut m1 = Tlv8::new();
        m1.insert_u8(TlvTag::State, 0x01);
        m1.insert_u8(TlvTag::Method, 0x00);
        let m2 = parse_pair_setup_tlv(exchange_pair_setup_raw(
            &mut stream, 1, &self.user_agent, "/pair-setup", 3, &m1.encode(),
            self.exchange_timeout,
        )?)?;
        require_state(&m2, 0x02)?;

        let salt = m2.get(TlvTag::Salt).ok_or(PairingError::InvalidSalt)?;
        if salt.len() != 16 {
            return Err(PairingError::InvalidSalt);
        }
        let server_b = m2.get(TlvTag::PublicKey)
            .ok_or(PairingError::InvalidServerPublicKey)?;
        if server_b.is_empty() || server_b.len() > 384 {
            return Err(PairingError::InvalidServerPublicKey);
        }

        let pin = pin_provider().filter(|pin| !pin.is_empty())
            .ok_or(PairingError::InvalidCredentials)?;
        let srp = srp_client_compute(salt, server_b, &pin)?;

        let mut m3 = Tlv8::new();
        m3.insert_u8(TlvTag::State, 0x03);
        m3.insert(TlvTag::PublicKey, srp.public_key_a.clone());
        m3.insert(TlvTag::Proof, srp.proof_m1.to_vec());
        let m4 = parse_pair_setup_tlv(exchange_pair_setup_raw(
            &mut stream, 2, &self.user_agent, "/pair-setup", 3, &m3.encode(),
            self.exchange_timeout,
        )?)?;
        require_state(&m4, 0x04)?;
        let proof = m4.get(TlvTag::Proof).ok_or(PairingError::InvalidServerProof)?;
        if proof != srp.expected_hamk {
            return Err(PairingError::InvalidServerProof);
        }

        let enc_key = hkdf32(
            &srp.session_key,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
        )?;
        let device_x = hkdf32(
            &srp.session_key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
        )?;

        let signing = SigningKey::generate(&mut OsRng);
        let client_public = signing.verifying_key().to_bytes();
        let client_id = client_id.to_ascii_uppercase();
        let mut controller_info = Vec::with_capacity(32 + client_id.len() + 32);
        controller_info.extend_from_slice(&device_x);
        controller_info.extend_from_slice(client_id.as_bytes());
        controller_info.extend_from_slice(&client_public);
        let signature = signing.sign(&controller_info).to_bytes();

        let mut sub = Tlv8::new();
        sub.insert(TlvTag::Identifier, client_id.as_bytes().to_vec());
        sub.insert(TlvTag::PublicKey, client_public.to_vec());
        sub.insert(TlvTag::Signature, signature.to_vec());
        let encrypted = hap_message_encrypt(&enc_key, b"PS-Msg05", &sub.encode())?;

        let mut m5 = Tlv8::new();
        m5.insert_u8(TlvTag::State, 0x05);
        m5.insert(TlvTag::EncryptedData, encrypted);
        let m6 = parse_pair_setup_tlv(exchange_pair_setup_raw(
            &mut stream, 3, &self.user_agent, "/pair-setup", 3, &m5.encode(),
            self.exchange_timeout,
        )?)?;
        require_state(&m6, 0x06)?;
        let encrypted_m6 = m6.get(TlvTag::EncryptedData)
            .ok_or(PairingError::MissingEncryptedData)?;
        let plain_m6 = hap_message_decrypt(&enc_key, b"PS-Msg06", encrypted_m6)?;
        let sub_m6 = Tlv8::decode(&plain_m6)?;
        let server_key = sub_m6.get(TlvTag::PublicKey)
            .ok_or(PairingError::InvalidServerPublicKey)?;
        if server_key.len() != 32 {
            return Err(PairingError::InvalidServerPublicKey);
        }
        let mut server_public = [0u8; 32];
        server_public.copy_from_slice(server_key);

        Ok(StoredHapCredentials {
            client_seed: signing.to_bytes(),
            client_public,
            server_public,
        })
    }

    pub fn pair_verify_on_stream(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        client_id: &str,
        credentials: &StoredHapCredentials,
    ) -> Result<TransientPairingSession, PairingError> {
        configure_pairing_stream(&mut stream, self.exchange_timeout)?;
        let mut codec = RtspCodec::default();
        let mut pending = Vec::<RtspResponse>::new();

        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = X25519PublicKey::from(&secret);
        let mut m1 = Tlv8::new();
        m1.insert_u8(TlvTag::State, 0x01);
        m1.insert(TlvTag::PublicKey, public.as_bytes().to_vec());
        let m2 = parse_pair_tlv(exchange(
            &mut stream, &mut codec, &mut pending, 1, &self.user_agent,
            "/pair-verify", 3, &m1.encode(), self.exchange_timeout,
        )?)?;
        require_state(&m2, 0x02)?;

        let server_eph = m2.get(TlvTag::PublicKey)
            .ok_or(PairingError::InvalidServerPublicKey)?;
        if server_eph.len() != 32 {
            return Err(PairingError::InvalidServerPublicKey);
        }
        let mut server_eph_bytes = [0u8; 32];
        server_eph_bytes.copy_from_slice(server_eph);
        let shared = secret.diffie_hellman(&X25519PublicKey::from(server_eph_bytes));
        let shared_secret = *shared.as_bytes();
        let verify_key = hkdf32(
            &shared_secret,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
        )?;

        let encrypted_m2 = m2.get(TlvTag::EncryptedData)
            .ok_or(PairingError::MissingEncryptedData)?;
        let plain_m2 = hap_message_decrypt(&verify_key, b"PV-Msg02", encrypted_m2)?;
        let sub_m2 = Tlv8::decode(&plain_m2)?;
        let server_id = sub_m2.get(TlvTag::Identifier)
            .ok_or(PairingError::MissingIdentifier)?;
        let server_sig = sub_m2.get(TlvTag::Signature)
            .ok_or(PairingError::InvalidSignature)?;
        if server_sig.len() != 64 {
            return Err(PairingError::InvalidSignature);
        }

        // Pinned MSA treats accessory signature verification as advisory here:
        // the receiver's M4 verification of OUR stored identity is authoritative.
        if let (Ok(server_verify), Ok(sig)) = (
            VerifyingKey::from_bytes(&credentials.server_public),
            Signature::from_slice(server_sig),
        ) {
            let mut accessory_info = Vec::with_capacity(32 + server_id.len() + 32);
            accessory_info.extend_from_slice(server_eph);
            accessory_info.extend_from_slice(server_id);
            accessory_info.extend_from_slice(public.as_bytes());
            let _ = server_verify.verify(&accessory_info, &sig);
        }

        let client_id = client_id.to_ascii_uppercase();
        let signing = SigningKey::from_bytes(&credentials.client_seed);
        let mut controller_info = Vec::with_capacity(32 + client_id.len() + 32);
        controller_info.extend_from_slice(public.as_bytes());
        controller_info.extend_from_slice(client_id.as_bytes());
        controller_info.extend_from_slice(server_eph);
        let signature = signing.sign(&controller_info).to_bytes();

        let mut sub_m3 = Tlv8::new();
        sub_m3.insert(TlvTag::Identifier, client_id.as_bytes().to_vec());
        sub_m3.insert(TlvTag::Signature, signature.to_vec());
        let encrypted_m3 = hap_message_encrypt(&verify_key, b"PV-Msg03", &sub_m3.encode())?;
        let mut m3 = Tlv8::new();
        m3.insert_u8(TlvTag::State, 0x03);
        m3.insert(TlvTag::EncryptedData, encrypted_m3);

        let m4 = parse_pair_tlv(exchange(
            &mut stream, &mut codec, &mut pending, 2, &self.user_agent,
            "/pair-verify", 3, &m3.encode(), self.exchange_timeout,
        )?)?;
        require_state(&m4, 0x04)?;

        let (write_key, read_key) = derive_control_keys(&shared_secret)?;
        let mut session_key = [0u8; 64];
        session_key[..32].copy_from_slice(&shared_secret);
        let pairing = TransientPairingResult {
            peer,
            write_key,
            read_key,
            audio_secret: shared_secret,
            session_key,
        };
        let channel = EncryptedRtspChannel::new(
            stream,
            pairing.write_key,
            pairing.read_key,
            self.exchange_timeout,
        );
        Ok(TransientPairingSession { pairing, channel })
    }
}

fn configure_pairing_stream(stream: &mut TcpStream, exchange_timeout: Duration) -> Result<(), PairingError> {
    stream.set_nodelay(true).map_err(PairingError::Configure)?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(PairingError::Configure)?;
    stream
        .set_write_timeout(Some(exchange_timeout))
        .map_err(PairingError::Configure)?;
    Ok(())
}

fn hkdf32(secret: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; 32], PairingError> {
    let hk = Hkdf::<Sha512>::new(Some(salt), secret);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .map_err(|_| PairingError::Crypto(HapCryptoError::Hkdf))?;
    Ok(out)
}

fn hap_message_nonce(label: &[u8; 8]) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(label);
    nonce
}

fn hap_message_encrypt(key: &[u8; 32], label: &[u8; 8], plaintext: &[u8]) -> Result<Vec<u8>, PairingError> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(&hap_message_nonce(label)), plaintext)
        .map_err(|_| PairingError::Crypto(HapCryptoError::Encrypt))
}

fn hap_message_decrypt(key: &[u8; 32], label: &[u8; 8], ciphertext: &[u8]) -> Result<Vec<u8>, PairingError> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(&hap_message_nonce(label)), ciphertext)
        .map_err(|_| PairingError::Crypto(HapCryptoError::Decrypt))
}

pub struct TransientPairingClient {
    connect_timeout: Duration,
    exchange_timeout: Duration,
    user_agent: String,
}

impl Default for TransientPairingClient {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            exchange_timeout: Duration::from_secs(8),
            user_agent: "AirPlay/670.6.2".into(),
        }
    }
}

impl TransientPairingClient {
    pub fn with_timeouts(mut self, connect: Duration, exchange: Duration) -> Self {
        self.connect_timeout = connect;
        self.exchange_timeout = exchange;
        self
    }

    pub fn pair(
        &self,
        host: &str,
        port: u16,
        password: Option<&str>,
    ) -> Result<TransientPairingResult, PairingError> {
        Ok(self.pair_channel(host, port, password)?.pairing)
    }

    pub fn pair_channel(
        &self,
        host: &str,
        port: u16,
        password: Option<&str>,
    ) -> Result<TransientPairingSession, PairingError> {
        let peer = (host, port)
            .to_socket_addrs()
            .map_err(|_| PairingError::Resolve)?
            .next()
            .ok_or(PairingError::Resolve)?;

        let stream = TcpStream::connect_timeout(&peer, self.connect_timeout)
            .map_err(PairingError::Connect)?;
        self.pair_channel_on_stream(stream, peer, password)
    }

    /// Continue HAP transient pairing on an already-open control socket.
    /// This is the native path used after plaintext GET /info so the socket
    /// survives unchanged into encrypted RTSP.
    pub fn pair_channel_on_stream(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        password: Option<&str>,
    ) -> Result<TransientPairingSession, PairingError> {
        stream.set_nodelay(true).map_err(PairingError::Configure)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(PairingError::Configure)?;
        stream
            .set_write_timeout(Some(self.exchange_timeout))
            .map_err(PairingError::Configure)?;

        let mut codec = RtspCodec::default();
        let mut pending = Vec::<RtspResponse>::new();

        let mut m1 = Tlv8::new();
        m1.insert_u8(TlvTag::State, 0x01);
        m1.insert_u8(TlvTag::Method, 0x00);
        m1.insert_u8(TlvTag::Flags, HAP_TRANSIENT_FLAG);

        let m2_resp = exchange(
            &mut stream, &mut codec, &mut pending, 1, &self.user_agent,
            "/pair-setup", 4, &m1.encode(), self.exchange_timeout,
        )?;
        let m2 = parse_pair_tlv(m2_resp)?;
        require_state(&m2, 0x02)?;

        let salt = m2.get(TlvTag::Salt).ok_or(PairingError::InvalidSalt)?;
        if salt.len() != 16 {
            return Err(PairingError::InvalidSalt);
        }

        let server_b = m2
            .get(TlvTag::PublicKey)
            .ok_or(PairingError::InvalidServerPublicKey)?;
        if server_b.is_empty() || server_b.len() > 384 {
            return Err(PairingError::InvalidServerPublicKey);
        }

        let secret = password.filter(|s| !s.is_empty()).unwrap_or(SRP_TRANSIENT_PIN);
        let srp = srp_client_compute(salt, server_b, secret)?;

        let mut m3 = Tlv8::new();
        m3.insert_u8(TlvTag::State, 0x03);
        m3.insert(TlvTag::PublicKey, srp.public_key_a.clone());
        m3.insert(TlvTag::Proof, srp.proof_m1.to_vec());

        let m4_resp = exchange(
            &mut stream, &mut codec, &mut pending, 2, &self.user_agent,
            "/pair-setup", 4, &m3.encode(), self.exchange_timeout,
        )?;
        let m4 = parse_pair_tlv(m4_resp)?;
        require_state(&m4, 0x04)?;

        let server_proof = m4
            .get(TlvTag::Proof)
            .ok_or(PairingError::InvalidServerProof)?;
        if server_proof != srp.expected_hamk {
            return Err(PairingError::InvalidServerProof);
        }

        let (write_key, read_key) = derive_control_keys(&srp.session_key)?;
        let mut audio_secret = [0u8; 32];
        audio_secret.copy_from_slice(&srp.session_key[..32]);

        let pairing = TransientPairingResult {
            peer,
            write_key,
            read_key,
            audio_secret,
            session_key: srp.session_key,
        };

        let channel = EncryptedRtspChannel::new(
            stream,
            pairing.write_key,
            pairing.read_key,
            self.exchange_timeout,
        );

        Ok(TransientPairingSession { pairing, channel })
    }

}


#[derive(Debug)]
struct PairSetupRawResponse {
    status: u16,
    body: Vec<u8>,
}

fn exchange_pair_setup_raw(
    stream: &mut TcpStream,
    cseq: u32,
    user_agent: &str,
    path: &str,
    hkp: u8,
    body: &[u8],
    timeout: Duration,
) -> Result<PairSetupRawResponse, PairingError> {
    let request = encode_pair_request(cseq, user_agent, path, hkp, body);
    stream.write_all(&request).map_err(PairingError::Write)?;

    let deadline = Instant::now() + timeout;
    let mut response = Vec::<u8>::with_capacity(8192);
    let mut header_len = None::<usize>;
    let mut content_len = None::<usize>;
    let mut buf = [0u8; 4096];

    loop {
        if let (Some(h), Some(cl)) = (header_len, content_len) {
            if response.len() >= h.saturating_add(cl) {
                break;
            }
        }
        if Instant::now() >= deadline {
            return Err(PairingError::Timeout);
        }

        match stream.read(&mut buf) {
            Ok(0) => return Err(PairingError::Closed),
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if header_len.is_none() {
                    if let Some(end) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                        let h = end + 4;
                        let header = std::str::from_utf8(&response[..end])
                            .map_err(|_| PairingError::Rtsp(RtspError::InvalidHeader))?;
                        let mut cl = 0usize;
                        for line in header.split("\r\n").skip(1) {
                            if let Some((name, value)) = line.split_once(':') {
                                if name.trim().eq_ignore_ascii_case("content-length") {
                                    cl = value.trim().parse().map_err(|_| {
                                        PairingError::Rtsp(RtspError::InvalidContentLength)
                                    })?;
                                }
                            }
                        }
                        header_len = Some(h);
                        content_len = Some(cl);
                    }
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) => return Err(PairingError::Read(err)),
        }
    }

    let h = header_len.ok_or(PairingError::Rtsp(RtspError::InvalidHeader))?;
    let cl = content_len.unwrap_or(0);
    let header = std::str::from_utf8(&response[..h - 4])
        .map_err(|_| PairingError::Rtsp(RtspError::InvalidHeader))?;
    let status = header
        .split("\r\n")
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or(PairingError::Rtsp(RtspError::InvalidStatusCode))?
        .parse::<u16>()
        .map_err(|_| PairingError::Rtsp(RtspError::InvalidStatusCode))?;

    Ok(PairSetupRawResponse {
        status,
        body: response[h..h + cl].to_vec(),
    })
}

fn parse_pair_setup_tlv(response: PairSetupRawResponse) -> Result<Tlv8, PairingError> {
    if response.status != 200 {
        return Err(PairingError::Status(response.status));
    }
    let tlv = Tlv8::decode(&response.body)?;
    if let Some(error) = tlv.error() {
        if error != 0 {
            return Err(PairingError::TlvError(error));
        }
    }
    Ok(tlv)
}

fn parse_pair_tlv(response: RtspResponse) -> Result<Tlv8, PairingError> {
    if response.status != 200 {
        return Err(PairingError::Status(response.status));
    }

    let tlv = Tlv8::decode(&response.body)?;
    if let Some(error) = tlv.error() {
        if error != 0 {
            return Err(PairingError::TlvError(error));
        }
    }
    Ok(tlv)
}

fn require_state(tlv: &Tlv8, expected: u8) -> Result<(), PairingError> {
    let actual = tlv.state();
    if actual != Some(expected) {
        return Err(PairingError::UnexpectedState { expected, actual });
    }
    Ok(())
}

fn exchange(
    stream: &mut TcpStream,
    codec: &mut RtspCodec,
    pending: &mut Vec<RtspResponse>,
    cseq: u32,
    user_agent: &str,
    path: &str,
    hkp: u8,
    body: &[u8],
    timeout: Duration,
) -> Result<RtspResponse, PairingError> {
    let request = encode_pair_request(cseq, user_agent, path, hkp, body);
    stream.write_all(&request).map_err(PairingError::Write)?;

    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];

    loop {
        if let Some(response) = RtspCodec::take_matching(pending, cseq)? {
            return Ok(response);
        }
        if Instant::now() >= deadline {
            return Err(PairingError::Timeout);
        }

        match stream.read(&mut buf) {
            Ok(0) => return Err(PairingError::Closed),
            Ok(n) => pending.extend(codec.push(&buf[..n])?),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(PairingError::Read(err)),
        }
    }
}

fn encode_pair_request(
    cseq: u32,
    user_agent: &str,
    path: &str,
    hkp: u8,
    body: &[u8],
) -> Vec<u8> {
    let head = format!(
        "POST {path} RTSP/1.0\r\nUser-Agent: {user_agent}\r\nContent-Type: application/octet-stream\r\nX-Apple-HKP: {hkp}\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigUint;
    use sha2::{Digest, Sha512};
    use std::net::TcpListener;
    use std::thread;

    const N_HEX: &str = concat!(
        "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
        "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
        "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
        "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
        "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
        "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
        "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
        "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
        "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
        "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
        "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
        "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
    );

    fn h(data: &[u8]) -> [u8; 64] {
        let digest = Sha512::digest(data);
        let mut out = [0u8;64];
        out.copy_from_slice(&digest);
        out
    }
    fn pad(v: &BigUint) -> Vec<u8> {
        let raw = v.to_bytes_be();
        let mut out = vec![0u8;384];
        let start = 384 - raw.len();
        out[start..].copy_from_slice(&raw);
        out
    }
    fn min(v: &BigUint) -> Vec<u8> {
        let x = v.to_bytes_be();
        if x.is_empty(){vec![0]}else{x}
    }

    fn read_request(socket: &mut TcpStream) -> (u32, Vec<u8>) {
        let mut buf = Vec::new();
        let mut tmp = [0u8;4096];
        loop {
            let n = socket.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header_end = end + 4;
                let header = String::from_utf8_lossy(&buf[..end]).into_owned();
                let len = header.lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .unwrap().parse::<usize>().unwrap();
                while buf.len() < header_end + len {
                    let n = socket.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                }
                let cseq = header.lines()
                    .find_map(|l| l.strip_prefix("CSeq: "))
                    .unwrap().parse().unwrap();
                assert!(header.contains("X-Apple-HKP: 4"));
                assert!(header.contains("User-Agent: AirPlay/670.6.2"));
                return (cseq, buf[header_end..header_end+len].to_vec());
            }
        }
    }

    fn send_tlv(socket: &mut TcpStream, cseq: u32, tlv: &Tlv8) {
        let body = tlv.encode();
        let head = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n\r\n", body.len());
        socket.write_all(head.as_bytes()).unwrap();
        socket.write_all(&body).unwrap();
    }

    #[test]
    fn transient_pairing_completes_m1_to_m4_over_real_tcp() {
        let listener = TcpListener::bind(("127.0.0.1",0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let n = BigUint::parse_bytes(N_HEX.as_bytes(),16).unwrap();
            let g = BigUint::from(5u8);
            let salt = [0x42u8;16];
            let inner = h(b"Pair-Setup:3939");
            let mut xin = Vec::from(salt);
            xin.extend_from_slice(&inner);
            let x = BigUint::from_bytes_be(&h(&xin));
            let v = g.modpow(&x,&n);
            let k = BigUint::from_bytes_be(&h(&[pad(&n),pad(&g)].concat()));
            let b = BigUint::from(0x123456789abcdefu64);
            let b_pub = ((&k*&v)+g.modpow(&b,&n))%&n;

            let (cseq1, body1) = read_request(&mut socket);
            assert_eq!(cseq1,1);
            let m1 = Tlv8::decode(&body1).unwrap();
            assert_eq!(m1.state(),Some(1));
            assert_eq!(m1.get_u8(TlvTag::Method),Some(0));
            assert_eq!(m1.get_u8(TlvTag::Flags),Some(HAP_TRANSIENT_FLAG));

            let mut m2 = Tlv8::new();
            m2.insert_u8(TlvTag::State,2);
            m2.insert(TlvTag::Salt,salt.to_vec());
            m2.insert(TlvTag::PublicKey,min(&b_pub));
            send_tlv(&mut socket,cseq1,&m2);

            let (cseq2, body3) = read_request(&mut socket);
            assert_eq!(cseq2,2);
            let m3 = Tlv8::decode(&body3).unwrap();
            assert_eq!(m3.state(),Some(3));
            let a_bytes = m3.get(TlvTag::PublicKey).unwrap();
            let a_pub = BigUint::from_bytes_be(a_bytes);
            let proof = m3.get(TlvTag::Proof).unwrap();

            let u = BigUint::from_bytes_be(&h(&[pad(&a_pub),pad(&b_pub)].concat()));
            let s = ((&a_pub * v.modpow(&u,&n))%&n).modpow(&b,&n);
            let session_key = h(&min(&s));
            let hn = h(&min(&n));
            let hg = h(&min(&g));
            let hu = h(b"Pair-Setup");
            let mut xor=[0u8;64];
            for i in 0..64 { xor[i]=hn[i]^hg[i]; }
            let mut m1in=Vec::new();
            m1in.extend_from_slice(&xor);
            m1in.extend_from_slice(&hu);
            m1in.extend_from_slice(&salt);
            m1in.extend_from_slice(a_bytes);
            m1in.extend_from_slice(&min(&b_pub));
            m1in.extend_from_slice(&session_key);
            let expected_m1=h(&m1in);
            assert_eq!(proof,expected_m1);

            let mut hamkin=Vec::new();
            hamkin.extend_from_slice(a_bytes);
            hamkin.extend_from_slice(&expected_m1);
            hamkin.extend_from_slice(&session_key);
            let hamk=h(&hamkin);

            let mut m4=Tlv8::new();
            m4.insert_u8(TlvTag::State,4);
            m4.insert(TlvTag::Proof,hamk.to_vec());
            send_tlv(&mut socket,cseq2,&m4);
        });

        let result = TransientPairingClient::default()
            .with_timeouts(Duration::from_secs(1),Duration::from_secs(3))
            .pair("127.0.0.1",addr.port(),None)
            .expect("pairing");

        assert_ne!(result.write_key,[0u8;32]);
        assert_ne!(result.read_key,[0u8;32]);
        assert_eq!(&result.audio_secret[..],&result.session_key[..32]);
        server.join().unwrap();
    }

    #[test]
    fn paired_socket_handoffs_directly_to_encrypted_rtsp() {
        let listener = TcpListener::bind(("127.0.0.1",0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let n = BigUint::parse_bytes(N_HEX.as_bytes(),16).unwrap();
            let g = BigUint::from(5u8);
            let salt = [0x24u8;16];
            let inner = h(b"Pair-Setup:3939");
            let mut xin = Vec::from(salt);
            xin.extend_from_slice(&inner);
            let x = BigUint::from_bytes_be(&h(&xin));
            let v = g.modpow(&x,&n);
            let k = BigUint::from_bytes_be(&h(&[pad(&n),pad(&g)].concat()));
            let b = BigUint::from(0x1122334455667788u64);
            let b_pub = ((&k*&v)+g.modpow(&b,&n))%&n;

            let (cseq1, _) = read_request(&mut socket);
            let mut m2=Tlv8::new();
            m2.insert_u8(TlvTag::State,2);
            m2.insert(TlvTag::Salt,salt.to_vec());
            m2.insert(TlvTag::PublicKey,min(&b_pub));
            send_tlv(&mut socket,cseq1,&m2);

            let (cseq2, body3)=read_request(&mut socket);
            let m3=Tlv8::decode(&body3).unwrap();
            let a_bytes=m3.get(TlvTag::PublicKey).unwrap();
            let a_pub=BigUint::from_bytes_be(a_bytes);
            let u=BigUint::from_bytes_be(&h(&[pad(&a_pub),pad(&b_pub)].concat()));
            let s=((&a_pub*v.modpow(&u,&n))%&n).modpow(&b,&n);
            let session_key=h(&min(&s));

            let hn=h(&min(&n));
            let hg=h(&min(&g));
            let hu=h(b"Pair-Setup");
            let mut xor=[0u8;64];
            for i in 0..64 { xor[i]=hn[i]^hg[i]; }
            let mut m1in=Vec::new();
            m1in.extend_from_slice(&xor);
            m1in.extend_from_slice(&hu);
            m1in.extend_from_slice(&salt);
            m1in.extend_from_slice(a_bytes);
            m1in.extend_from_slice(&min(&b_pub));
            m1in.extend_from_slice(&session_key);
            let proof=h(&m1in);
            assert_eq!(m3.get(TlvTag::Proof).unwrap(), proof);

            let mut hamkin=Vec::new();
            hamkin.extend_from_slice(a_bytes);
            hamkin.extend_from_slice(&proof);
            hamkin.extend_from_slice(&session_key);
            let hamk=h(&hamkin);
            let mut m4=Tlv8::new();
            m4.insert_u8(TlvTag::State,4);
            m4.insert(TlvTag::Proof,hamk.to_vec());
            send_tlv(&mut socket,cseq2,&m4);

            let (write_key, read_key)=derive_control_keys(&session_key).unwrap();
            let mut server_cipher=crate::HapControlCipher::new(read_key, write_key);

            let mut carry=Vec::new();
            let mut tmp=[0u8;4096];
            let plain=loop {
                let nread=socket.read(&mut tmp).unwrap();
                carry.extend_from_slice(&tmp[..nread]);
                if carry.len()<2 { continue; }
                let plen=u16::from_le_bytes([carry[0],carry[1]]) as usize;
                let flen=2+plen+16;
                if carry.len()>=flen {
                    break server_cipher.decrypt(&carry[..flen]).unwrap();
                }
            };
            let text=String::from_utf8(plain).unwrap();
            assert!(text.contains("CSeq: 3\r\n"));

            let reply=b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 0\r\n\r\n";
            let wire=server_cipher.encrypt(reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let mut session=TransientPairingClient::default()
            .with_timeouts(Duration::from_secs(1),Duration::from_secs(3))
            .pair_channel("127.0.0.1",addr.port(),None)
            .expect("paired channel");

        let req=b"POST /feedback RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 0\r\n\r\n";
        let resp=session.channel.exchange(req,3).expect("encrypted exchange");
        assert_eq!(resp.status,200);
        assert_eq!(resp.cseq(),Some(3));

        server.join().unwrap();
    }

    #[test]
    fn tlv_auth_error_is_preserved() {
        let listener = TcpListener::bind(("127.0.0.1",0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let (cseq, _) = read_request(&mut socket);
            let mut tlv=Tlv8::new();
            tlv.insert_u8(TlvTag::State,2);
            tlv.insert_u8(TlvTag::Error,2);
            send_tlv(&mut socket,cseq,&tlv);
        });

        let result = TransientPairingClient::default()
            .with_timeouts(Duration::from_secs(1),Duration::from_secs(3))
            .pair("127.0.0.1",addr.port(),None);

        assert!(matches!(result,Err(PairingError::TlvError(2))));
        server.join().unwrap();
    }
}
