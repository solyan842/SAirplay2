use crate::{BufferedMediaSender, BufferedSendError, EncryptedRtspChannel, EncryptedRtspError, RtspRequest};
use plist::{Dictionary, Value};
use std::io::Write;

#[derive(Debug, Clone)]
pub struct FlushBufferedConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub dacp_id: String,
    pub active_remote: String,
}

#[derive(Debug)]
pub enum FlushBufferedError {
    Sender(BufferedSendError),
    Transport(EncryptedRtspError),
    Plist(plist::Error),
    Status(u16),
}

impl From<BufferedSendError> for FlushBufferedError {
    fn from(value: BufferedSendError) -> Self { Self::Sender(value) }
}
impl From<EncryptedRtspError> for FlushBufferedError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}
impl From<plist::Error> for FlushBufferedError {
    fn from(value: plist::Error) -> Self { Self::Plist(value) }
}

pub fn build_flushbuffered_plist(
    flush_until_seq: u16,
    flush_until_ts: u32,
) -> Result<Vec<u8>, FlushBufferedError> {
    let mut root = Dictionary::new();
    root.insert(
        "flushUntilSeq".into(),
        Value::Integer((flush_until_seq as u64).into()),
    );
    root.insert(
        "flushUntilTS".into(),
        Value::Integer((flush_until_ts as u64).into()),
    );
    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

/// Pinned-MSA FLUSHBUFFERED path.
///
/// The data sender is quiesced first. A wholly unwritten pending frame is
/// dropped; a partially-written frame is given up to one second to finish.
/// The flush boundary names the sender's current next sequence/timestamp.
/// The previous anchor is invalid after the verb regardless of RTSP status.
pub fn send_flushbuffered<W: Write>(
    channel: &mut EncryptedRtspChannel,
    sender: &mut BufferedMediaSender<W>,
    config: &FlushBufferedConfig,
) -> Result<bool, FlushBufferedError> {
    let _fully_quiesced = sender.quiesce_for_flush()?;
    let state = sender.state();
    let body = build_flushbuffered_plist(state.sequence, state.timestamp)?;

    let request = RtspRequest {
        method: "FLUSHBUFFERED".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };

    let result = channel.exchange(&request.encode(), config.cseq);
    sender.clear_anchored();

    let response = result?;
    if response.status != 200 {
        return Err(FlushBufferedError::Status(response.status));
    }
    Ok(_fully_quiesced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HapControlCipher, RtpState};
    use std::io::{Cursor, Read, Write as IoWrite};
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
            let frame_len = 2 + plen + 16;
            if carry.len() >= frame_len {
                return cipher.decrypt(&carry[..frame_len]).unwrap();
            }
        }
    }

    #[test]
    fn plist_contains_exact_flush_boundary_fields() {
        let body = build_flushbuffered_plist(0x1234, 0xAABBCCDD).unwrap();
        let value = Value::from_reader(Cursor::new(&body)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(
            root.get("flushUntilSeq").and_then(Value::as_unsigned_integer),
            Some(0x1234)
        );
        assert_eq!(
            root.get("flushUntilTS").and_then(Value::as_unsigned_integer),
            Some(0xAABBCCDD)
        );
    }

    #[test]
    fn encrypted_flush_uses_current_sender_boundary_and_clears_anchor() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0xA3u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let plain = read_one_frame(&mut socket, &mut cipher);
            let header_end = plain.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let header = std::str::from_utf8(&plain[..header_end]).unwrap();
            assert!(header.starts_with(
                "FLUSHBUFFERED rtsp://127.0.0.1/session RTSP/1.0\r\n"
            ));
            assert!(header.contains("CSeq: 11\r\n"));

            let body = Value::from_reader(Cursor::new(&plain[header_end..])).unwrap();
            let root = body.as_dictionary().unwrap();
            assert_eq!(
                root.get("flushUntilSeq").and_then(Value::as_unsigned_integer),
                Some(7)
            );
            assert_eq!(
                root.get("flushUntilTS").and_then(Value::as_unsigned_integer),
                Some(55_000)
            );

            let reply = b"RTSP/1.0 200 OK\r\nCSeq: 11\r\nContent-Length: 0\r\n\r\n";
            let wire = cipher.encrypt(reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let control_stream = TcpStream::connect(addr).unwrap();
        control_stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(
            control_stream,
            key,
            key,
            Duration::from_secs(2),
        );

        let data_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let data_addr = data_listener.local_addr().unwrap();
        let data_client = TcpStream::connect(data_addr).unwrap();
        let (_data_server, _) = data_listener.accept().unwrap();
        let mut sender = BufferedMediaSender::new(
            data_client,
            RtpState::new(7, 55_000, 0),
            [0x44u8; 32],
        );
        sender.mark_anchored();

        let config = FlushBufferedConfig {
            cseq: 11,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        };

        assert!(send_flushbuffered(&mut channel, &mut sender, &config).unwrap());
        assert!(!sender.is_anchored());
        server.join().unwrap();
    }
}
