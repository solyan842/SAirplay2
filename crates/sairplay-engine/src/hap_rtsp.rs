use crate::{HapControlCipher, HapCryptoError, RtspCodec, RtspError, RtspResponse};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum EncryptedRtspError {
    Write(std::io::Error),
    Read(std::io::Error),
    Timeout,
    Closed,
    Crypto(HapCryptoError),
    Rtsp(RtspError),
}

impl From<HapCryptoError> for EncryptedRtspError {
    fn from(value: HapCryptoError) -> Self { Self::Crypto(value) }
}
impl From<RtspError> for EncryptedRtspError {
    fn from(value: RtspError) -> Self { Self::Rtsp(value) }
}

pub struct EncryptedRtspChannel {
    stream: TcpStream,
    cipher: HapControlCipher,
    rtsp: RtspCodec,
    pending: Vec<RtspResponse>,
    encrypted_carry: Vec<u8>,
    exchange_timeout: Duration,
}

impl EncryptedRtspChannel {
    pub fn new(
        stream: TcpStream,
        write_key: [u8; 32],
        read_key: [u8; 32],
        exchange_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            cipher: HapControlCipher::new(write_key, read_key),
            rtsp: RtspCodec::default(),
            pending: Vec::new(),
            encrypted_carry: Vec::new(),
            exchange_timeout,
        }
    }

    pub fn exchange(
        &mut self,
        request: &[u8],
        expected_cseq: u32,
    ) -> Result<RtspResponse, EncryptedRtspError> {
        self.exchange_with_timeout(request, expected_cseq, self.exchange_timeout)
    }

    pub fn exchange_with_timeout(
        &mut self,
        request: &[u8],
        expected_cseq: u32,
        timeout: Duration,
    ) -> Result<RtspResponse, EncryptedRtspError> {
        let wire = self.cipher.encrypt(request)?;
        self.stream.write_all(&wire).map_err(EncryptedRtspError::Write)?;

        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];

        loop {
            if let Some(response) = RtspCodec::take_matching(&mut self.pending, expected_cseq)? {
                return Ok(response);
            }

            self.decrypt_complete_frames()?;

            if let Some(response) = RtspCodec::take_matching(&mut self.pending, expected_cseq)? {
                return Ok(response);
            }

            if Instant::now() >= deadline {
                return Err(EncryptedRtspError::Timeout);
            }

            match self.stream.read(&mut buf) {
                Ok(0) => return Err(EncryptedRtspError::Closed),
                Ok(n) => self.encrypted_carry.extend_from_slice(&buf[..n]),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(err) => return Err(EncryptedRtspError::Read(err)),
            }
        }
    }

    fn decrypt_complete_frames(&mut self) -> Result<(), EncryptedRtspError> {
        loop {
            if self.encrypted_carry.len() < 2 {
                return Ok(());
            }

            let plain_len =
                u16::from_le_bytes([self.encrypted_carry[0], self.encrypted_carry[1]]) as usize;
            if plain_len > 1024 {
                return Err(EncryptedRtspError::Crypto(HapCryptoError::InvalidFrame));
            }

            let frame_len = 2 + plain_len + 16;
            if self.encrypted_carry.len() < frame_len {
                return Ok(());
            }

            let frame = self.encrypted_carry[..frame_len].to_vec();
            let plaintext = self.cipher.decrypt(&frame)?;
            self.encrypted_carry.drain(..frame_len);

            let parsed = self.rtsp.push(&plaintext)?;
            self.pending.extend(parsed);
        }
    }

    pub fn write_only_with_timeout(
        &mut self,
        request: &[u8],
        timeout: Duration,
    ) -> Result<(), EncryptedRtspError> {
        let wire = self.cipher.encrypt(request)?;
        self.stream
            .set_write_timeout(Some(timeout))
            .map_err(EncryptedRtspError::Write)?;
        let result = self
            .stream
            .write_all(&wire)
            .map_err(EncryptedRtspError::Write);
        let _ = self.stream.set_write_timeout(Some(self.exchange_timeout));
        result
    }

    pub fn pending_responses(&self) -> usize {
        self.pending.len()
    }

    pub fn encrypted_carry_len(&self) -> usize {
        self.encrypted_carry.len()
    }

    pub fn write_counter(&self) -> u64 {
        self.cipher.write_counter()
    }

    pub fn read_counter(&self) -> u64 {
        self.cipher.read_counter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn response(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn encrypted_exchange_survives_fragmented_frames_and_stale_cseq() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x55u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

            let mut server_cipher = HapControlCipher::new(key, key);
            let mut carry = Vec::new();
            let mut buf = [0u8; 4096];
            let request_plain = loop {
                let n = socket.read(&mut buf).unwrap();
                carry.extend_from_slice(&buf[..n]);
                if carry.len() < 2 {
                    continue;
                }
                let plen = u16::from_le_bytes([carry[0], carry[1]]) as usize;
                let frame_len = 2 + plen + 16;
                if carry.len() >= frame_len {
                    break server_cipher.decrypt(&carry[..frame_len]).unwrap();
                }
            };

            let request_text = String::from_utf8(request_plain).unwrap();
            assert!(request_text.contains("CSeq: 7\r\n"));

            let stale = response(6, b"late");
            let stale_wire = server_cipher.encrypt(&stale).unwrap();

            let current = response(7, b"ok");
            let current_wire = server_cipher.encrypt(&current).unwrap();

            socket.write_all(&stale_wire[..5]).unwrap();
            socket.write_all(&stale_wire[5..]).unwrap();

            let split = current_wire.len() / 2;
            socket.write_all(&current_wire[..split]).unwrap();
            socket.write_all(&current_wire[split..]).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();

        let mut channel =
            EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let request = b"POST /feedback RTSP/1.0\r\nCSeq: 7\r\nContent-Length: 0\r\n\r\n";
        let response = channel.exchange(request, 7).unwrap();

        assert_eq!(response.cseq(), Some(7));
        assert_eq!(response.body, b"ok");
        assert_eq!(channel.pending_responses(), 1);
        assert_eq!(channel.write_counter(), 1);
        assert_eq!(channel.read_counter(), 2);

        server.join().unwrap();
    }

    #[test]
    fn encrypted_frame_carry_is_preserved_until_complete() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x66u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut server_cipher = HapControlCipher::new(key, key);
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).unwrap();

            let wire = server_cipher.encrypt(&response(1, b"fragmented")).unwrap();
            socket.write_all(&wire[..1]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            socket.write_all(&wire[1..3]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            socket.write_all(&wire[3..]).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel =
            EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let req = b"GET /info RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n";
        let resp = channel.exchange(req, 1).unwrap();

        assert_eq!(resp.body, b"fragmented");
        assert_eq!(channel.encrypted_carry_len(), 0);

        server.join().unwrap();
    }
}
