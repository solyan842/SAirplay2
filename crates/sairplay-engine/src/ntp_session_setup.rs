use crate::{
    EncryptedRtspChannel, EncryptedRtspError, NativeConnectError, NativeConnectFlow, RtspRequest,
};
use plist::{Dictionary, Value};
use std::io::Cursor;

#[derive(Debug, Clone)]
pub struct NtpSessionSetupConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub session_uuid: String,
    pub device_id: Option<String>,
    pub timing_port: u16,
    pub dacp_id: String,
    pub active_remote: String,
}

#[derive(Debug)]
pub enum NtpSessionSetupError {
    Flow(NativeConnectError),
    Transport(EncryptedRtspError),
    Status(u16),
    Plist(plist::Error),
    InvalidRoot,
    MissingEventPort,
    InvalidEventPort,
}

impl From<NativeConnectError> for NtpSessionSetupError {
    fn from(value: NativeConnectError) -> Self { Self::Flow(value) }
}
impl From<EncryptedRtspError> for NtpSessionSetupError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}
impl From<plist::Error> for NtpSessionSetupError {
    fn from(value: plist::Error) -> Self { Self::Plist(value) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtpSessionSetupResult {
    pub event_port: u16,
}

pub fn build_ntp_session_plist(
    session_uuid: &str,
    device_id: Option<&str>,
    timing_port: u16,
) -> Result<Vec<u8>, NtpSessionSetupError> {
    let mut root = Dictionary::new();
    if let Some(device_id) = device_id.filter(|value| !value.is_empty()) {
        root.insert("deviceID".into(), Value::String(device_id.to_string()));
    }
    root.insert("sessionUUID".into(), Value::String(session_uuid.to_string()));
    root.insert(
        "timingPort".into(),
        Value::Integer((timing_port as u64).into()),
    );
    root.insert("timingProtocol".into(), Value::String("NTP".into()));

    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

pub fn parse_event_port(body: &[u8]) -> Result<u16, NtpSessionSetupError> {
    let value = Value::from_reader(Cursor::new(body))?;
    let root = value
        .as_dictionary()
        .ok_or(NtpSessionSetupError::InvalidRoot)?;

    let event = root
        .get("eventPort")
        .and_then(Value::as_unsigned_integer)
        .ok_or(NtpSessionSetupError::MissingEventPort)?;

    if !(1024..=65535).contains(&event) {
        return Err(NtpSessionSetupError::InvalidEventPort);
    }

    Ok(event as u16)
}

pub fn setup_ntp_session(
    flow: &mut NativeConnectFlow,
    channel: &mut EncryptedRtspChannel,
    config: &NtpSessionSetupConfig,
) -> Result<NtpSessionSetupResult, NtpSessionSetupError> {
    // Preserve ordering: Session SETUP may only run after live timing readiness.
    if flow.phase() != crate::NativePhase::TimingReady {
        flow.session_setup()?;
        unreachable!("session_setup succeeds only from TimingReady");
    }

    let body = build_ntp_session_plist(
        &config.session_uuid,
        config.device_id.as_deref(),
        config.timing_port,
    )?;

    let request = RtspRequest {
        method: "SETUP".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: Some(config.dacp_id.clone()),
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };

    let response = channel.exchange(&request.encode(), config.cseq)?;
    if response.status != 200 {
        return Err(NtpSessionSetupError::Status(response.status));
    }

    let event_port = parse_event_port(&response.body)?;

    // Advance only after successful encrypted exchange and valid eventPort.
    flow.session_setup()?;

    Ok(NtpSessionSetupResult { event_port })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HapControlCipher, NativePhase};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    fn timing_ready_flow() -> NativeConnectFlow {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow
    }

    fn response_plist(event_port: u64) -> Vec<u8> {
        let mut root = Dictionary::new();
        root.insert("eventPort".into(), Value::Integer(event_port.into()));
        let mut out = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut out).unwrap();
        out
    }

    fn encrypted_response(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
            body.len()
        ).into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn read_one_hap_frame(socket: &mut TcpStream, cipher: &mut HapControlCipher) -> Vec<u8> {
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
    fn plist_contains_only_source_confirmed_ntp_session_fields() {
        let body = build_ntp_session_plist(
            "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
            Some("AA:BB:CC:DD:EE:FF:00:11"),
            54321,
        ).unwrap();

        let value = Value::from_reader(Cursor::new(&body)).unwrap();
        let dict = value.as_dictionary().unwrap();

        assert_eq!(dict.len(), 4);
        assert_eq!(
            dict.get("timingProtocol").and_then(Value::as_string),
            Some("NTP")
        );
        assert_eq!(
            dict.get("timingPort").and_then(Value::as_unsigned_integer),
            Some(54321)
        );
        assert_eq!(
            dict.get("sessionUUID").and_then(Value::as_string),
            Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")
        );
        assert_eq!(
            dict.get("deviceID").and_then(Value::as_string),
            Some("AA:BB:CC:DD:EE:FF:00:11")
        );
    }

    #[test]
    fn encrypted_setup_advances_only_after_valid_200_and_event_port() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x71u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);

            let plain = read_one_hap_frame(&mut socket, &mut cipher);
            let mut codec = crate::RtspCodec::default();
            let req = String::from_utf8(plain.clone()).unwrap();

            assert!(req.starts_with("SETUP rtsp://127.0.0.1/session RTSP/1.0\r\n"));
            assert!(req.contains("CSeq: 4\r\n"));
            assert!(req.contains("Content-Type: application/x-apple-binary-plist\r\n"));

            let header_end = plain.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let body = &plain[header_end..];
            let value = Value::from_reader(Cursor::new(body)).unwrap();
            let dict = value.as_dictionary().unwrap();
            assert_eq!(
                dict.get("timingProtocol").and_then(Value::as_string),
                Some("NTP")
            );
            assert_eq!(
                dict.get("timingPort").and_then(Value::as_unsigned_integer),
                Some(45678)
            );

            // Exercise the same response parser shape as the client.
            assert!(codec.push(b"RTSP/1.0 200 OK\r\nCSeq: 99\r\nContent-Length: 0\r\n\r\n").is_ok());

            let reply = encrypted_response(4, &response_plist(7000));
            let wire = cipher.encrypt(&reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(
            stream,
            key,
            key,
            Duration::from_secs(2),
        );

        let mut flow = timing_ready_flow();
        let config = NtpSessionSetupConfig {
            cseq: 4,
            session_uri: "rtsp://127.0.0.1/session".into(),
            session_uuid: "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into(),
            device_id: Some("AA:BB:CC:DD:EE:FF:00:11".into()),
            timing_port: 45678,
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        };

        let result = setup_ntp_session(&mut flow, &mut channel, &config).unwrap();
        assert_eq!(result.event_port, 7000);
        assert_eq!(flow.phase(), NativePhase::SessionSetup);

        server.join().unwrap();
    }

    #[test]
    fn invalid_event_port_keeps_flow_at_timing_ready() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x72u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let _ = read_one_hap_frame(&mut socket, &mut cipher);

            let reply = encrypted_response(4, &response_plist(80));
            let wire = cipher.encrypt(&reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(
            stream,
            key,
            key,
            Duration::from_secs(2),
        );

        let mut flow = timing_ready_flow();
        let config = NtpSessionSetupConfig {
            cseq: 4,
            session_uri: "rtsp://127.0.0.1/session".into(),
            session_uuid: "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into(),
            device_id: None,
            timing_port: 45678,
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        };

        assert!(matches!(
            setup_ntp_session(&mut flow, &mut channel, &config),
            Err(NtpSessionSetupError::InvalidEventPort)
        ));
        assert_eq!(flow.phase(), NativePhase::TimingReady);

        server.join().unwrap();
    }
}
