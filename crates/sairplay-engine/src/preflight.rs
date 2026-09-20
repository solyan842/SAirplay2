use crate::{Ap2Info, Ap2InfoError, RtspCodec, RtspError, RtspRequest, RtspResponse};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum PreflightError {
    Resolve,
    Connect(std::io::Error),
    Configure(std::io::Error),
    Write(std::io::Error),
    Read(std::io::Error),
    Rtsp(RtspError),
    Timeout,
    Closed,
    Status(u16),
    Info(Ap2InfoError),
}

impl From<RtspError> for PreflightError {
    fn from(value: RtspError) -> Self {
        Self::Rtsp(value)
    }
}

impl From<Ap2InfoError> for PreflightError {
    fn from(value: Ap2InfoError) -> Self {
        Self::Info(value)
    }
}

#[derive(Debug, Clone)]
pub struct PreflightResult {
    pub peer: SocketAddr,
    pub response: RtspResponse,
    pub info: Ap2Info,
}

pub struct Ap2PreflightClient {
    connect_timeout: Duration,
    exchange_timeout: Duration,
    dacp_id: String,
    active_remote: String,
}

impl Ap2PreflightClient {
    pub fn new(dacp_id: impl Into<String>, active_remote: impl Into<String>) -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            exchange_timeout: Duration::from_secs(8),
            dacp_id: dacp_id.into(),
            active_remote: active_remote.into(),
        }
    }

    pub fn with_timeouts(mut self, connect: Duration, exchange: Duration) -> Self {
        self.connect_timeout = connect;
        self.exchange_timeout = exchange;
        self
    }

    pub fn get_info(
        &self,
        host: &str,
        port: u16,
    ) -> Result<PreflightResult, PreflightError> {
        let (_stream, result) = self.open_info_connection(host, port)?;
        Ok(result)
    }

    /// Opens the native control TCP socket and performs plaintext GET /info
    /// without closing it. The returned stream is intended to continue into
    /// HAP pairing and then encrypted RTSP on the same receiver connection.
    pub fn open_info_connection(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(TcpStream, PreflightResult), PreflightError> {
        let peer = (host, port)
            .to_socket_addrs()
            .map_err(|_| PreflightError::Resolve)?
            .next()
            .ok_or(PreflightError::Resolve)?;

        let mut stream = TcpStream::connect_timeout(&peer, self.connect_timeout)
            .map_err(PreflightError::Connect)?;
        stream
            .set_nodelay(true)
            .map_err(PreflightError::Configure)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(PreflightError::Configure)?;
        stream
            .set_write_timeout(Some(self.exchange_timeout))
            .map_err(PreflightError::Configure)?;

        // Source-native client starts its RTSP counter at zero.
        let cseq = 0u32;
        let request = RtspRequest::get_info(
            cseq,
            self.dacp_id.clone(),
            self.active_remote.clone(),
        );
        stream
            .write_all(&request.encode())
            .map_err(PreflightError::Write)?;

        let deadline = Instant::now() + self.exchange_timeout;
        let mut codec = RtspCodec::default();
        let mut pending = Vec::<RtspResponse>::new();
        let mut buf = [0u8; 4096];

        loop {
            if let Some(response) = RtspCodec::take_matching(&mut pending, cseq)? {
                if response.status != 200 {
                    return Err(PreflightError::Status(response.status));
                }
                let info = Ap2Info::parse(&response.body)?;
                return Ok((stream, PreflightResult {
                    peer,
                    response,
                    info,
                }));
            }

            if Instant::now() >= deadline {
                return Err(PreflightError::Timeout);
            }

            match stream.read(&mut buf) {
                Ok(0) => return Err(PreflightError::Closed),
                Ok(n) => pending.extend(codec.push(&buf[..n])?),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(err) => return Err(PreflightError::Read(err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::{Dictionary, Value};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn info_body() -> Vec<u8> {
        let mut ext = Dictionary::new();
        ext.insert(
            "audioStream".into(),
            Value::Array(vec![Value::Integer(18u64.into())]),
        );
        let mut root = Dictionary::new();
        root.insert(
            "supportedAudioFormatsExtended".into(),
            Value::Dictionary(ext),
        );

        let mut body = Vec::new();
        Value::Dictionary(root)
            .to_writer_binary(&mut body)
            .unwrap();
        body
    }

    fn response(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn real_tcp_preflight_ignores_stale_cseq_and_parses_info() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();

            let mut req = [0u8; 2048];
            let n = socket.read(&mut req).unwrap();
            let req_text = String::from_utf8_lossy(&req[..n]);
            assert!(req_text.starts_with("GET /info RTSP/1.0\r\n"));
            assert!(req_text.contains("CSeq: 0\r\n"));

            let stale = response(99, b"stale");
            socket.write_all(&stale[..11]).unwrap();
            socket.write_all(&stale[11..]).unwrap();

            let current = response(0, &info_body());
            let split = current.len() / 2;
            socket.write_all(&current[..split]).unwrap();
            socket.write_all(&current[split..]).unwrap();
        });

        let client = Ap2PreflightClient::new("AABBCCDDEEFF0011", "123456789")
            .with_timeouts(Duration::from_secs(1), Duration::from_secs(2));

        let result = client
            .get_info("127.0.0.1", addr.port())
            .expect("preflight");

        assert_eq!(result.response.cseq(), Some(0));
        assert!(result.info.realtime.known);
        assert!(result.info.realtime.extended);
        assert!(result.info.realtime.advertises(crate::ALAC_44100_16_2));

        server.join().unwrap();
    }

    #[test]
    fn non_200_info_is_reported_not_parsed_as_capabilities() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = socket.read(&mut req).unwrap();
            socket
                .write_all(b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let client = Ap2PreflightClient::new("AABBCCDDEEFF0011", "123456789")
            .with_timeouts(Duration::from_secs(1), Duration::from_secs(2));

        assert!(matches!(
            client.get_info("127.0.0.1", addr.port()),
            Err(PreflightError::Status(401))
        ));

        server.join().unwrap();
    }
}
