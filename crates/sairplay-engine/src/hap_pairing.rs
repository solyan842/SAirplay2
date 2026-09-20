use crate::{
    derive_control_keys, srp_client_compute, HapCryptoError, RtspCodec, RtspError, RtspResponse,
    SrpError, Tlv8, Tlv8Error, TlvTag, HAP_TRANSIENT_FLAG, SRP_TRANSIENT_PIN,
};
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
        let peer = (host, port)
            .to_socket_addrs()
            .map_err(|_| PairingError::Resolve)?
            .next()
            .ok_or(PairingError::Resolve)?;

        let mut stream = TcpStream::connect_timeout(&peer, self.connect_timeout)
            .map_err(PairingError::Connect)?;
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
            &mut stream,
            &mut codec,
            &mut pending,
            1,
            &self.user_agent,
            "/pair-setup",
            4,
            &m1.encode(),
            self.exchange_timeout,
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
            &mut stream,
            &mut codec,
            &mut pending,
            2,
            &self.user_agent,
            "/pair-setup",
            4,
            &m3.encode(),
            self.exchange_timeout,
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

        Ok(TransientPairingResult {
            peer,
            write_key,
            read_key,
            audio_secret,
            session_key: srp.session_key,
        })
    }
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
