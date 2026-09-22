use crate::{
    Ap2AudioFormat, EncryptedRtspChannel, EncryptedRtspError, NativeConnectError,
    NativeConnectFlow, RtspRequest,
};
use plist::{Dictionary, Value};
use std::io::Cursor;

pub const REALTIME_STREAM_TYPE: u64 = 96;
pub const ALAC_CODEC_TYPE: u64 = 2;
pub const FRAMES_PER_PACKET: u64 = 352;
pub const LATENCY_MIN_FRAMES: u64 = 11_025;
pub const LATENCY_MAX_FRAMES: u64 = 88_200;

#[derive(Debug, Clone)]
pub struct RealtimeStreamSetupConfig {
    pub cseq: u32,
    pub session_uri: String,
    pub dacp_id: String,
    pub active_remote: String,
    pub local_data_port: u16,
    pub local_control_port: u16,
    pub audio_secret: [u8; 32],
    pub stream_connection_id: u32,
    pub audio_format: Ap2AudioFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPorts {
    pub data_port: u16,
    pub control_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealtimeStreamSetupResult {
    pub ports: StreamPorts,
    pub latency_min: Option<u32>,
    pub latency_max: Option<u32>,
}

#[derive(Debug)]
pub enum StreamSetupError {
    Flow(NativeConnectError),
    Transport(EncryptedRtspError),
    Status(u16),
    Plist(plist::Error),
    InvalidRoot,
    MissingStreams,
    InvalidStream,
    MissingDataPort,
    InvalidDataPort,
    MissingControlPort,
    InvalidControlPort,
}

impl From<NativeConnectError> for StreamSetupError {
    fn from(value: NativeConnectError) -> Self { Self::Flow(value) }
}
impl From<EncryptedRtspError> for StreamSetupError {
    fn from(value: EncryptedRtspError) -> Self { Self::Transport(value) }
}
impl From<plist::Error> for StreamSetupError {
    fn from(value: plist::Error) -> Self { Self::Plist(value) }
}

pub fn build_realtime_stream_plist(
    local_data_port: u16,
    local_control_port: u16,
    audio_secret: &[u8; 32],
    stream_connection_id: u32,
    audio_format: Ap2AudioFormat,
) -> Result<Vec<u8>, StreamSetupError> {
    let mut stream = Dictionary::new();
    stream.insert(
        "audioFormat".into(),
        Value::Integer(audio_format.audio_format_code().into()),
    );
    stream.insert("audioMode".into(), Value::String("default".into()));
    stream.insert("controlPort".into(), Value::Integer((local_control_port as u64).into()));
    stream.insert("ct".into(), Value::Integer(ALAC_CODEC_TYPE.into()));
    stream.insert("dataPort".into(), Value::Integer((local_data_port as u64).into()));
    stream.insert("isMedia".into(), Value::Boolean(true));
    stream.insert("latencyMax".into(), Value::Integer(LATENCY_MAX_FRAMES.into()));
    stream.insert("latencyMin".into(), Value::Integer(LATENCY_MIN_FRAMES.into()));
    stream.insert("shk".into(), Value::Data(audio_secret.to_vec()));
    stream.insert("spf".into(), Value::Integer(FRAMES_PER_PACKET.into()));
    stream.insert(
        "sr".into(),
        Value::Integer((audio_format.sample_rate as u64).into()),
    );
    stream.insert(
        "streamConnectionID".into(),
        Value::Integer((stream_connection_id as u64).into()),
    );
    stream.insert("supportsDynamicStreamID".into(), Value::Boolean(false));
    stream.insert("type".into(), Value::Integer(REALTIME_STREAM_TYPE.into()));

    let mut root = Dictionary::new();
    root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));

    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

pub fn parse_stream_setup_response(
    body: &[u8],
) -> Result<RealtimeStreamSetupResult, StreamSetupError> {
    let value = Value::from_reader(Cursor::new(body))?;
    let root = value.as_dictionary().ok_or(StreamSetupError::InvalidRoot)?;
    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .ok_or(StreamSetupError::MissingStreams)?;
    let stream = streams
        .first()
        .and_then(Value::as_dictionary)
        .ok_or(StreamSetupError::InvalidStream)?;

    let data = stream
        .get("dataPort")
        .and_then(Value::as_unsigned_integer)
        .ok_or(StreamSetupError::MissingDataPort)?;
    if !(1024..=65535).contains(&data) {
        return Err(StreamSetupError::InvalidDataPort);
    }

    let control = stream
        .get("controlPort")
        .and_then(Value::as_unsigned_integer)
        .ok_or(StreamSetupError::MissingControlPort)?;
    if !(1024..=65535).contains(&control) {
        return Err(StreamSetupError::InvalidControlPort);
    }

    let latency_min = stream
        .get("latencyMin")
        .and_then(Value::as_unsigned_integer)
        .filter(|v| *v > 0 && *v <= u32::MAX as u64)
        .map(|v| v as u32);
    let latency_max = stream
        .get("latencyMax")
        .and_then(Value::as_unsigned_integer)
        .filter(|v| *v > 0 && *v <= u32::MAX as u64)
        .map(|v| v as u32);

    Ok(RealtimeStreamSetupResult {
        ports: StreamPorts {
            data_port: data as u16,
            control_port: control as u16,
        },
        latency_min,
        latency_max,
    })
}

pub fn parse_stream_ports(body: &[u8]) -> Result<StreamPorts, StreamSetupError> {
    Ok(parse_stream_setup_response(body)?.ports)
}

pub fn setup_realtime_stream(
    flow: &mut NativeConnectFlow,
    channel: &mut EncryptedRtspChannel,
    config: &RealtimeStreamSetupConfig,
) -> Result<RealtimeStreamSetupResult, StreamSetupError> {
    if flow.phase() != crate::NativePhase::Recorded {
        flow.stream_setup()?;
        unreachable!("stream_setup succeeds only from Recorded");
    }

    let body = build_realtime_stream_plist(
        config.local_data_port,
        config.local_control_port,
        &config.audio_secret,
        config.stream_connection_id,
        config.audio_format,
    )?;

    let request = RtspRequest {
        method: "SETUP".into(),
        uri: config.session_uri.clone(),
        cseq: config.cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: config.dacp_id.clone(),
        active_remote: config.active_remote.clone(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };

    let response = channel.exchange(&request.encode(), config.cseq)?;
    if response.status != 200 {
        return Err(StreamSetupError::Status(response.status));
    }

    let result = parse_stream_setup_response(&response.body)?;

    flow.stream_setup()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HapControlCipher, NativePhase};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
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

    fn response_body(data_port: u64, control_port: u64) -> Vec<u8> {
        let mut stream = Dictionary::new();
        // Insert in reverse/alphabetical-sensitive order intentionally:
        // parser must use keys, never positional integer guessing.
        stream.insert("controlPort".into(), Value::Integer(control_port.into()));
        stream.insert("dataPort".into(), Value::Integer(data_port.into()));
        stream.insert("latencyMin".into(), Value::Integer(22050u64.into()));
        stream.insert("latencyMax".into(), Value::Integer(66150u64.into()));
        let mut root = Dictionary::new();
        root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let mut out = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut out).unwrap();
        out
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

    fn response(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
            body.len()
        ).into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn realtime_plist_defaults_match_16_441_type96_contract() {
        let secret = [0xABu8; 32];
        let body = build_realtime_stream_plist(
            50000,
            50001,
            &secret,
            0x12345678,
            Ap2AudioFormat::ALAC_44100_16_STEREO,
        ).unwrap();
        let value = Value::from_reader(Cursor::new(&body)).unwrap();
        let root = value.as_dictionary().unwrap();
        let stream = root.get("streams").unwrap().as_array().unwrap()[0]
            .as_dictionary().unwrap();

        assert_eq!(stream.get("audioFormat").and_then(Value::as_unsigned_integer), Some(crate::ALAC_44100_16_2));
        assert_eq!(stream.get("ct").and_then(Value::as_unsigned_integer), Some(2));
        assert_eq!(stream.get("type").and_then(Value::as_unsigned_integer), Some(96));
        assert_eq!(stream.get("sr").and_then(Value::as_unsigned_integer), Some(44_100));
        assert_eq!(stream.get("spf").and_then(Value::as_unsigned_integer), Some(352));
        assert_eq!(stream.get("dataPort").and_then(Value::as_unsigned_integer), Some(50000));
        assert_eq!(stream.get("controlPort").and_then(Value::as_unsigned_integer), Some(50001));
        assert_eq!(stream.get("latencyMin").and_then(Value::as_unsigned_integer), Some(11_025));
        assert_eq!(stream.get("latencyMax").and_then(Value::as_unsigned_integer), Some(88_200));
        assert_eq!(stream.get("isMedia").and_then(Value::as_boolean), Some(true));
        assert_eq!(stream.get("supportsDynamicStreamID").and_then(Value::as_boolean), Some(false));
        assert_eq!(stream.get("shk").and_then(Value::as_data), Some(secret.as_slice()));
    }

    #[test]
    fn realtime_plist_emits_source_24_48_audio_format() {
        let secret = [0xCDu8; 32];
        let body = build_realtime_stream_plist(
            50000,
            50001,
            &secret,
            0x12345678,
            Ap2AudioFormat::ALAC_48000_24_STEREO,
        ).unwrap();
        let value = Value::from_reader(Cursor::new(&body)).unwrap();
        let stream = value
            .as_dictionary().unwrap()
            .get("streams").unwrap().as_array().unwrap()[0]
            .as_dictionary().unwrap();
        assert_eq!(
            stream.get("audioFormat").and_then(Value::as_unsigned_integer),
            Some(crate::ALAC_48000_24_2)
        );
        assert_eq!(
            stream.get("sr").and_then(Value::as_unsigned_integer),
            Some(48_000)
        );
        assert_eq!(stream.get("spf").and_then(Value::as_unsigned_integer), Some(352));
        assert_eq!(stream.get("ct").and_then(Value::as_unsigned_integer), Some(2));
    }

    #[test]
    fn response_ports_are_parsed_by_key_not_position() {
        let ports = parse_stream_ports(&response_body(60000, 60001)).unwrap();
        assert_eq!(ports, StreamPorts { data_port: 60000, control_port: 60001 });
        let parsed = parse_stream_setup_response(&response_body(60000, 60001)).unwrap();
        assert_eq!(parsed.latency_min, Some(22050));
        assert_eq!(parsed.latency_max, Some(66150));
    }

    #[test]
    fn encrypted_stream_setup_advances_recorded_to_stream_setup() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x73u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut cipher = HapControlCipher::new(key, key);

            let plain = read_one_frame(&mut socket, &mut cipher);
            let header_end = plain.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let header = std::str::from_utf8(&plain[..header_end]).unwrap();

            assert!(header.starts_with("SETUP rtsp://127.0.0.1/session RTSP/1.0\r\n"));
            assert!(header.contains("CSeq: 6\r\n"));
            assert!(header.contains("Content-Type: application/x-apple-binary-plist\r\n"));

            let value = Value::from_reader(Cursor::new(&plain[header_end..])).unwrap();
            let root = value.as_dictionary().unwrap();
            let stream = root.get("streams").unwrap().as_array().unwrap()[0]
                .as_dictionary().unwrap();
            assert_eq!(stream.get("type").and_then(Value::as_unsigned_integer), Some(96));
            assert_eq!(stream.get("audioFormat").and_then(Value::as_unsigned_integer), Some(ALAC_44100_16_2));
            assert_eq!(stream.get("sr").and_then(Value::as_unsigned_integer), Some(44_100));
            assert_eq!(stream.get("spf").and_then(Value::as_unsigned_integer), Some(352));

            let wire = cipher.encrypt(&response(6, &response_body(61000, 61001))).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(2));

        let mut flow = recorded_flow();
        let config = RealtimeStreamSetupConfig {
            cseq: 6,
            session_uri: "rtsp://127.0.0.1/session".into(),
            dacp_id: "AABBCCDDEEFF0011".into(),
            active_remote: "123456789".into(),
            local_data_port: 50000,
            local_control_port: 50001,
            audio_secret: [0xAAu8; 32],
            stream_connection_id: 0x12345678,
            audio_format: Ap2AudioFormat::ALAC_44100_16_STEREO,
        };

        let result = setup_realtime_stream(&mut flow, &mut channel, &config).unwrap();
        assert_eq!(result.ports, StreamPorts { data_port: 61000, control_port: 61001 });
        assert_eq!(flow.phase(), NativePhase::StreamSetup);

        server.join().unwrap();
    }

    #[test]
    fn missing_remote_control_port_blocks_stream_ready_state() {
        let mut stream = Dictionary::new();
        stream.insert("dataPort".into(), Value::Integer(61000u64.into()));
        let mut root = Dictionary::new();
        root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let mut body = Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut body).unwrap();

        assert!(matches!(
            parse_stream_ports(&body),
            Err(StreamSetupError::MissingControlPort)
        ));
    }
}
