use crate::{
    open_event_channel, prepare_realtime_media, send_record, setup_ntp_session,
    start_ntp_timing_gate, Ap2PreflightClient, EventChannel, MediaHandshakeConfig,
    NativeConnectFlow, NativePhase, NtpSessionSetupConfig, NtpTimingResponder,
    PairingError, PreflightError, RealtimeMediaSender, RecordConfig, RtpState,
    TransientPairingClient,
};
use rand::RngCore;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

#[cfg(windows)]
use crate::{WindowsAudioWorker, WindowsAudioWorkerError};

#[derive(Debug, Clone)]
pub struct NativeSessionConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub dacp_id: String,
    pub active_remote: String,
    pub lead_frames: u32,
}

impl NativeSessionConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            password: None,
            dacp_id: "A1B2C3D4E5F60708".into(),
            active_remote: "123456789".into(),
            lead_frames: 11_025,
        }
    }
}

#[derive(Debug)]
pub enum NativeSessionError {
    Preflight(PreflightError),
    Pairing(PairingError),
    Flow(String),
    Timing(String),
    SessionSetup(String),
    Event(String),
    Record(String),
    Media(String),
    LocalAddress(std::io::Error),
    #[cfg(windows)]
    Audio(WindowsAudioWorkerError),
}

impl fmt::Display for NativeSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preflight(e) => write!(f, "preflight failed: {e:?}"),
            Self::Pairing(e) => write!(f, "pairing failed: {e:?}"),
            Self::Flow(e) => write!(f, "native flow failed: {e}"),
            Self::Timing(e) => write!(f, "timing failed: {e}"),
            Self::SessionSetup(e) => write!(f, "session setup failed: {e}"),
            Self::Event(e) => write!(f, "event channel failed: {e}"),
            Self::Record(e) => write!(f, "record failed: {e}"),
            Self::Media(e) => write!(f, "media setup failed: {e}"),
            Self::LocalAddress(e) => write!(f, "local address failed: {e}"),
            #[cfg(windows)]
            Self::Audio(e) => write!(f, "Windows audio failed: {e}"),
        }
    }
}

impl std::error::Error for NativeSessionError {}

pub struct NativeSession {
    flow: NativeConnectFlow,
    control: crate::EncryptedRtspChannel,
    _timing: NtpTimingResponder,
    _event: EventChannel,
    sender: Option<RealtimeMediaSender>,
    lead_frames: u32,
    #[cfg(windows)]
    audio_worker: Option<WindowsAudioWorker>,
}

impl NativeSession {
    pub fn connect(config: &NativeSessionConfig) -> Result<Self, NativeSessionError> {
        let mut flow = NativeConnectFlow::default();

        // 1) One TCP connection: plaintext /info, then HAP, then encrypted RTSP.
        let preflight = Ap2PreflightClient::new(
            config.dacp_id.clone(),
            config.active_remote.clone(),
        );
        let (stream, info) = preflight
            .open_info_connection(&config.host, config.port)
            .map_err(NativeSessionError::Preflight)?;

        flow.tcp_connected()
            .map_err(|e| NativeSessionError::Flow(format!("{e:?}")))?;
        flow.info_loaded()
            .map_err(|e| NativeSessionError::Flow(format!("{e:?}")))?;

        if info.info.realtime.known && !info.info.realtime.advertises(crate::ALAC_44100_16_2) {
            return Err(NativeSessionError::Flow(
                "receiver explicitly does not advertise ALAC 16/44.1 stereo realtime".into(),
            ));
        }

        let local_addr = stream.local_addr().map_err(NativeSessionError::LocalAddress)?;
        let receiver_ip = info.peer.ip();

        let pairing_client = TransientPairingClient::default();
        let pairing = pairing_client
            .pair_channel_on_stream(
                stream,
                info.peer,
                config.password.as_deref(),
            )
            .map_err(NativeSessionError::Pairing)?;

        flow.paired()
            .map_err(|e| NativeSessionError::Flow(format!("{e:?}")))?;

        let audio_secret = pairing.pairing.audio_secret;
        let mut control = pairing.channel;

        // Source current native path uses random 32-bit session id in both
        // session URL and realtime streamConnectionID/SSRC for NTP.
        let mut rng = rand::thread_rng();
        let session_id = rng.next_u32();
        let sequence = rng.next_u32() as u16;
        let rtp_timestamp = rng.next_u32();
        let session_uuid = random_uuid_upper(&mut rng);
        let session_uri = format_session_uri(local_addr.ip(), session_id);

        // 2) Timing must be live before encrypted Session SETUP.
        let timing_bind = SocketAddr::new(local_addr.ip(), 0);
        let timing = start_ntp_timing_gate(&mut flow, timing_bind)
            .map_err(|e| NativeSessionError::Timing(format!("{e:?}")))?;
        let timing_port = timing
            .port()
            .map_err(NativeSessionError::LocalAddress)?;

        // Source keeps RTSP CSeq independent from the HAP pair-setup CSeqs:
        // GET /info consumes RTSP CSeq 0, so encrypted control resumes at 1.
        let setup = NtpSessionSetupConfig {
            cseq: 1,
            session_uri: session_uri.clone(),
            session_uuid,
            device_id: dacp_device_id(&config.dacp_id),
            timing_port,
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
        };
        let setup_result = setup_ntp_session(&mut flow, &mut control, &setup)
            .map_err(|e| NativeSessionError::SessionSetup(format!("{e:?}")))?;

        // 3) Keep-open reverse event TCP.
        let event = open_event_channel(
            &mut flow,
            receiver_ip,
            setup_result.event_port,
            &audio_secret,
            Duration::from_secs(3),
        )
        .map_err(|e| NativeSessionError::Event(format!("{e:?}")))?;

        // 4) RECORD before realtime stream SETUP.
        let record = RecordConfig {
            cseq: 2,
            session_uri: session_uri.clone(),
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
        };
        send_record(&mut flow, &mut control, &record)
            .map_err(|e| NativeSessionError::Record(format!("{e:?}")))?;

        // 5) Bind live UDP ports, advertise them, parse receiver ports, attach.
        let media = MediaHandshakeConfig {
            bind_ip: local_addr.ip(),
            receiver_ip,
            cseq: 3,
            session_uri,
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
            audio_secret,
            stream_connection_id: session_id,
        };
        let media = prepare_realtime_media(&mut flow, &mut control, &media)
            .map_err(|e| NativeSessionError::Media(format!("{e:?}")))?;

        // Only now is transport ready enough to allow audio capture.
        flow.ready()
            .map_err(|e| NativeSessionError::Flow(format!("{e:?}")))?;

        let rtp = RtpState::new(sequence, rtp_timestamp, session_id);
        let sender = RealtimeMediaSender::new(media.transport, rtp, audio_secret);

        Ok(Self {
            flow,
            control,
            _timing: timing,
            _event: event,
            sender: Some(sender),
            lead_frames: config.lead_frames,
            #[cfg(windows)]
            audio_worker: None,
        })
    }

    pub fn phase(&self) -> NativePhase {
        self.flow.phase()
    }

    pub fn is_ready(&self) -> bool {
        self.flow.phase() == NativePhase::Ready
    }

    pub fn control_channel(&mut self) -> &mut crate::EncryptedRtspChannel {
        &mut self.control
    }

    #[cfg(windows)]
    pub fn start_windows_audio(&mut self) -> Result<(), NativeSessionError> {
        if !self.is_ready() {
            return Err(NativeSessionError::Flow(
                "audio capture cannot start before native transport is Ready".into(),
            ));
        }
        if self.audio_worker.as_ref().is_some_and(|worker| worker.is_running()) {
            return Ok(());
        }

        let sender = self.sender.take().ok_or_else(|| {
            NativeSessionError::Flow("realtime sender is already owned by audio worker".into())
        })?;

        match WindowsAudioWorker::start(sender, self.lead_frames) {
            Ok(worker) => {
                self.audio_worker = Some(worker);
                Ok(())
            }
            Err(error) => Err(NativeSessionError::Audio(error)),
        }
    }

    #[cfg(windows)]
    pub fn audio_running(&self) -> bool {
        self.audio_worker.as_ref().is_some_and(|worker| worker.is_running())
    }

    #[cfg(windows)]
    pub fn audio_error(&self) -> Option<String> {
        self.audio_worker.as_ref().and_then(WindowsAudioWorker::last_error)
    }

    #[cfg(windows)]
    pub fn stop_windows_audio(&mut self) {
        if let Some(mut worker) = self.audio_worker.take() {
            worker.stop();
        }
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        // Stop the realtime producer first. Only after its thread is joined may
        // timing/event/control resources be released by normal field drops.
        #[cfg(windows)]
        self.stop_windows_audio();
    }
}

fn format_session_uri(local_ip: IpAddr, session_id: u32) -> String {
    match local_ip {
        IpAddr::V4(ip) => format!("rtsp://{ip}/{session_id}"),
        IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session_id}"),
    }
}

fn dacp_device_id(dacp_id: &str) -> Option<String> {
    let compact: String = dacp_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if compact.len() != 16 {
        return None;
    }

    let bytes = (0..8)
        .map(|i| &compact[i * 2..i * 2 + 2])
        .collect::<Vec<_>>();
    Some(bytes.join(":").to_ascii_uppercase())
}

fn random_uuid_upper(rng: &mut impl RngCore) -> String {
    let mut b = [0u8; 16];
    rng.fill_bytes(&mut b);
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn session_uri_uses_source_shape_and_ipv6_brackets() {
        assert_eq!(
            format_session_uri(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 42),
            "rtsp://192.168.1.20/42"
        );
        assert_eq!(
            format_session_uri(IpAddr::V6(Ipv6Addr::LOCALHOST), 42),
            "rtsp://[::1]/42"
        );
    }

    #[test]
    fn dacp_id_becomes_eight_byte_colon_device_id() {
        assert_eq!(
            dacp_device_id("A1B2C3D4E5F60708").as_deref(),
            Some("A1:B2:C3:D4:E5:F6:07:08")
        );
        assert!(dacp_device_id("1234").is_none());
    }

    #[test]
    fn uuid_shape_matches_native_session_uuid_format() {
        let mut rng = StdRng::seed_from_u64(7);
        let uuid = random_uuid_upper(&mut rng);
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[8], b'-');
        assert_eq!(uuid.as_bytes()[13], b'-');
        assert_eq!(uuid.as_bytes()[18], b'-');
        assert_eq!(uuid.as_bytes()[23], b'-');
    }
}
