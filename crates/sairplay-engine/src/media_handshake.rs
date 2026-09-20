use crate::{
    setup_realtime_stream, EncryptedRtspChannel, MediaTransport, MediaTransportError,
    NativeConnectFlow, RealtimeStreamSetupConfig, StreamPorts, StreamSetupError,
};
use std::net::IpAddr;

#[derive(Debug, Clone)]
pub struct MediaHandshakeConfig {
    pub bind_ip: IpAddr,
    pub receiver_ip: IpAddr,
    pub cseq: u32,
    pub session_uri: String,
    pub dacp_id: String,
    pub active_remote: String,
    pub audio_secret: [u8; 32],
    pub stream_connection_id: u32,
}

#[derive(Debug)]
pub enum MediaHandshakeError {
    Transport(MediaTransportError),
    Setup(StreamSetupError),
}

impl From<MediaTransportError> for MediaHandshakeError {
    fn from(value: MediaTransportError) -> Self { Self::Transport(value) }
}
impl From<StreamSetupError> for MediaHandshakeError {
    fn from(value: StreamSetupError) -> Self { Self::Setup(value) }
}

pub struct MediaHandshakeResult {
    pub transport: MediaTransport,
    pub remote_ports: StreamPorts,
    pub latency_min: Option<u32>,
    pub latency_max: Option<u32>,
}

pub fn prepare_realtime_media(
    flow: &mut NativeConnectFlow,
    channel: &mut EncryptedRtspChannel,
    config: &MediaHandshakeConfig,
) -> Result<MediaHandshakeResult, MediaHandshakeError> {
    // 1) Bind and keep both UDP sockets alive before advertising their ports.
    let mut transport = MediaTransport::bind(config.bind_ip)?;
    let local = transport.local_ports()?;

    // 2) Advertise exactly those bound ports in realtime stream SETUP.
    let setup = RealtimeStreamSetupConfig {
        cseq: config.cseq,
        session_uri: config.session_uri.clone(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        local_data_port: local.data_port,
        local_control_port: local.control_port,
        audio_secret: config.audio_secret,
        stream_connection_id: config.stream_connection_id,
    };
    let setup_result = setup_realtime_stream(flow, channel, &setup)?;
    let remote_ports = setup_result.ports;

    // 3) Only after a valid SETUP response attach the receiver endpoints.
    transport.attach_remote(config.receiver_ip, remote_ports);

    Ok(MediaHandshakeResult {
        transport,
        remote_ports,
        latency_min: setup_result.latency_min,
        latency_max: setup_result.latency_max,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HapControlCipher, NativePhase};
    use plist::{Dictionary, Value};
    use std::io::{Cursor, Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
    use std::thread;
    use std::time::Duration;

    fn recorded_flow() -> NativeConnectFlow {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow.session_setup().unwrap();
        flow.event_channel_open().unwrap();
        flow.recorded().unwrap();
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

    fn stream_response_body(data_port: u16, control_port: u16) -> Vec<u8> {
        let mut stream = Dictionary::new();
        stream.insert("controlPort".into(), Value::Integer((control_port as u64).into()));
        stream.insert("dataPort".into(), Value::Integer((data_port as u64).into()));
        let mut root = Dictionary::new();
        root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let mut out = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut out).unwrap();
        out
    }

    fn response(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
            body.len()
        ).into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn handshake_advertises_live_local_ports_then_attaches_remote_ports() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let remote_data = data_rx.local_addr().unwrap().port();
        let remote_ctrl = ctrl_rx.local_addr().unwrap().port();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x81u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);

            let plain = read_one_frame(&mut socket, &mut cipher);
            let header_end = plain.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let value = Value::from_reader(Cursor::new(&plain[header_end..])).unwrap();
            let root = value.as_dictionary().unwrap();
            let stream = root.get("streams").unwrap().as_array().unwrap()[0]
                .as_dictionary().unwrap();

            let local_data = stream.get("dataPort").and_then(Value::as_unsigned_integer).unwrap() as u16;
            let local_ctrl = stream.get("controlPort").and_then(Value::as_unsigned_integer).unwrap() as u16;

            // Prove the advertised ports are actually bound at SETUP time.
            assert!(UdpSocket::bind((Ipv4Addr::LOCALHOST, local_data)).is_err());
            assert!(UdpSocket::bind((Ipv4Addr::LOCALHOST, local_ctrl)).is_err());

            let wire = cipher.encrypt(&response(
                7,
                &stream_response_body(remote_data, remote_ctrl),
            )).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let mut flow = recorded_flow();
        let config = MediaHandshakeConfig {
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            receiver_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            cseq: 7,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
            audio_secret: [0xAAu8; 32],
            stream_connection_id: 0x11223344,
        };

        let result = prepare_realtime_media(&mut flow, &mut channel, &config).unwrap();

        assert_eq!(flow.phase(), NativePhase::StreamSetup);
        assert_eq!(
            result.remote_ports,
            StreamPorts { data_port: remote_data, control_port: remote_ctrl }
        );

        result.transport.send_data(b"rtp-ready").unwrap();
        result.transport.send_control(b"ctrl-ready").unwrap();

        let mut buf = [0u8; 64];
        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..dn], b"rtp-ready");
        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..cn], b"ctrl-ready");

        server.join().unwrap();
    }

    #[test]
    fn setup_failure_drops_bound_transport_and_does_not_attach_remote() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x82u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);
            let _ = read_one_frame(&mut socket, &mut cipher);
            let reply = b"RTSP/1.0 500 Error\r\nCSeq: 7\r\nContent-Length: 0\r\n\r\n";
            let wire = cipher.encrypt(reply).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let mut flow = recorded_flow();
        let config = MediaHandshakeConfig {
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            receiver_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            cseq: 7,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
            audio_secret: [0xAAu8; 32],
            stream_connection_id: 0x11223344,
        };

        assert!(matches!(
            prepare_realtime_media(&mut flow, &mut channel, &config),
            Err(MediaHandshakeError::Setup(StreamSetupError::Status(500)))
        ));
        assert_eq!(flow.phase(), NativePhase::Recorded);

        server.join().unwrap();
    }
}
