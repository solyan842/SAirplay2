use crate::{
    EncryptedRtspChannel, EncryptedRtspError, NativeConnectError, NativeConnectFlow, RtspRequest,
};

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub dacp_id: String,
    pub active_remote: String,
}

#[derive(Debug)]
pub enum RecordError {
    Flow(NativeConnectError),
    Transport(EncryptedRtspError),
    Status(u16),
}

impl From<NativeConnectError> for RecordError {
    fn from(value: NativeConnectError) -> Self { Self::Flow(value) }
}
impl From<EncryptedRtspError> for RecordError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}

pub fn send_record(
    flow: &mut NativeConnectFlow,
    channel: &mut EncryptedRtspChannel,
    config: &RecordConfig,
) -> Result<(), RecordError> {
    if flow.phase() != crate::NativePhase::EventChannelOpen {
        flow.recorded()?;
        unreachable!("recorded succeeds only from EventChannelOpen");
    }

    let request = RtspRequest {
        method: "RECORD".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: Some(config.dacp_id.clone()),
        content_type: None,
        body: Vec::new(),
    };

    let response = channel.exchange(&request.encode(), config.cseq)?;
    if response.status != 200 {
        return Err(RecordError::Status(response.status));
    }

    flow.recorded()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HapControlCipher, NativePhase};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    fn event_open_flow() -> NativeConnectFlow {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow.session_setup().unwrap();
        flow.event_channel_open().unwrap();
        flow
    }

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

    fn response(cseq: u32, status: u16, reason: &str) -> Vec<u8> {
        format!(
            "RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n"
        ).into_bytes()
    }

    #[test]
    fn encrypted_record_is_empty_body_on_session_uri_and_advances_flow() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x61u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);

            let plain = read_one_frame(&mut socket, &mut cipher);
            let text = String::from_utf8(plain).unwrap();

            assert!(text.starts_with("RECORD rtsp://127.0.0.1/session RTSP/1.0\r\n"));
            assert!(text.contains("CSeq: 5\r\n"));
            assert!(text.contains("Content-Length: 0\r\n\r\n"));
            assert!(!text.contains("Content-Type:"));

            let wire = cipher.encrypt(&response(5, 200, "OK")).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let mut flow = event_open_flow();
        let config = RecordConfig {
            cseq: 5,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        };

        send_record(&mut flow, &mut channel, &config).unwrap();
        assert_eq!(flow.phase(), NativePhase::Recorded);

        server.join().unwrap();
    }

    #[test]
    fn non_200_record_keeps_flow_at_event_channel_open() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x62u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let _ = read_one_frame(&mut socket, &mut cipher);
            let wire = cipher.encrypt(&response(5, 453, "Not Enough Bandwidth")).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let mut flow = event_open_flow();
        let config = RecordConfig {
            cseq: 5,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        };

        assert!(matches!(
            send_record(&mut flow, &mut channel, &config),
            Err(RecordError::Status(453))
        ));
        assert_eq!(flow.phase(), NativePhase::EventChannelOpen);

        server.join().unwrap();
    }

    #[test]
    fn record_is_rejected_before_event_channel_open() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x63u8; 32];
        let server = thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(1));

        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow.session_setup().unwrap();

        let config = RecordConfig {
            cseq: 5,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
        };

        assert!(matches!(
            send_record(&mut flow, &mut channel, &config),
            Err(RecordError::Flow(_))
        ));
        assert_eq!(flow.phase(), NativePhase::SessionSetup);

        server.join().unwrap();
    }
}
