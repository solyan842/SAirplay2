use crate::{EncryptedRtspChannel, EncryptedRtspError, RtspRequest};
use plist::Value;

#[derive(Debug, Clone)]
pub struct SetPeersConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub receiver_address: String,
    pub local_address: String,
    pub dacp_id: String,
    pub active_remote: String,
}

#[derive(Debug)]
pub enum SetPeersError {
    Transport(EncryptedRtspError),
    Plist(plist::Error),
}

impl From<EncryptedRtspError> for SetPeersError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}
impl From<plist::Error> for SetPeersError {
    fn from(value: plist::Error) -> Self { Self::Plist(value) }
}

pub fn build_setpeers_plist(
    receiver_address: &str,
    local_address: &str,
) -> Result<Vec<u8>, SetPeersError> {
    // Upstream sends a BARE array root: [receiver, us].
    let root = Value::Array(vec![
        Value::String(receiver_address.to_string()),
        Value::String(local_address.to_string()),
    ]);
    let mut out = Vec::new();
    root.to_writer_binary(&mut out)?;
    Ok(out)
}

pub fn send_setpeers(
    channel: &mut EncryptedRtspChannel,
    config: &SetPeersConfig,
) -> Result<u16, SetPeersError> {
    let request = RtspRequest {
        method: "SETPEERS".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body: build_setpeers_plist(&config.receiver_address, &config.local_address)?,
    };

    let response = channel.exchange(&request.encode(), config.cseq)?;
    // Match upstream ap2_client.c: transport failure is fatal; any received
    // RTSP status is logged/returned but does not abort solely on status code.
    Ok(response.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HapControlCipher;
    use std::io::{Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    fn read_one_frame(socket: &mut TcpStream, cipher: &mut HapControlCipher) -> Vec<u8> {
        let mut carry = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).unwrap();
            carry.extend_from_slice(&buf[..n]);
            if carry.len() < 2 { continue; }
            let plen = u16::from_le_bytes([carry[0], carry[1]]) as usize;
            let total = 2 + plen + 16;
            if carry.len() >= total {
                return cipher.decrypt(&carry[..total]).unwrap();
            }
        }
    }

    #[test]
    fn setpeers_is_bare_receiver_then_local_array_and_cseq4() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x91u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let plain = read_one_frame(&mut socket, &mut cipher);
            let header_end = plain.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let header = std::str::from_utf8(&plain[..header_end]).unwrap();
            assert!(header.starts_with("SETPEERS rtsp://127.0.0.1/session RTSP/1.0\r\n"));
            assert!(header.contains("CSeq: 4\r\n"));
            assert!(!header.contains("Client-Instance:"));

            let value = Value::from_reader(Cursor::new(&plain[header_end..])).unwrap();
            let arr = value.as_array().unwrap();
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[0].as_string(), Some("127.0.0.2"));
            assert_eq!(arr[1].as_string(), Some("127.0.0.1"));

            let response = b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nContent-Length: 0\r\n\r\n";
            socket.write_all(&cipher.encrypt(response).unwrap()).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));
        let status = send_setpeers(&mut channel, &SetPeersConfig {
            cseq: 4,
            session_uri: "rtsp://127.0.0.1/session".into(),
            receiver_address: "127.0.0.2".into(),
            local_address: "127.0.0.1".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        }).unwrap();

        assert_eq!(status, 200);
        server.join().unwrap();
    }
}
