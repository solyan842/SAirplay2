use crate::{
    open_event_channel, prepare_realtime_media, send_record, setup_ntp_session,
    send_setpeers, send_teardown, setup_ptp_session, start_ntp_timing_gate, Ap2PreflightClient,
    EventChannel, FeedbackWorker, MediaHandshakeConfig, NativeConnectFlow, NativePhase,
    NtpSessionSetupConfig, NtpTimingResponder, PairingError, PreflightError, PtpEngine,
    PtpSessionSetupConfig, RealtimeMediaSender, RecordConfig, RetransmitRing,
    NativeVolumeControl, RetransmitStats, RetransmitWorker, RtpState, SetPeersConfig, TransientPairingClient,
    VolumeSetResult, set_native_volume,
};
use rand::RngCore;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
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
    pub supports_ptp: bool,
    pub follow_receiver_clock: bool,
    pub apple_model: bool,
    pub receiver_name: String,
    pub initial_volume: Option<u8>,
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
            supports_ptp: false,
            follow_receiver_clock: false,
            apple_model: false,
            receiver_name: "SAirplay2 Receiver".into(),
            initial_volume: None,
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
    Feedback(std::io::Error),
    Volume(String),
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
            Self::Feedback(e) => write!(f, "feedback worker failed: {e}"),
            Self::Volume(e) => write!(f, "volume setup failed: {e}"),
            #[cfg(windows)]
            Self::Audio(e) => write!(f, "Windows audio failed: {e}"),
        }
    }
}

impl std::error::Error for NativeSessionError {}

pub struct NativeSession {
    flow: NativeConnectFlow,
    control: crate::SharedRtspControl,
    next_cseq: crate::SharedCseq,
    feedback: FeedbackWorker,
    retransmit: Option<RetransmitWorker>,
    _ntp_timing: Option<NtpTimingResponder>,
    _ptp_timing: Option<PtpEngine>,
    event: Option<EventChannel>,
    sender: Option<RealtimeMediaSender>,
    session_uri: String,
    dacp_id: String,
    active_remote: String,
    lead_frames: u32,
    latency_max: Option<u32>,
    rtp_offset: u32,
    cold_start_delay_ms: u64,
    initial_volume_result: Option<VolumeSetResult>,
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
        let session_uuid = random_uuid_upper(&mut rng);
        let session_uri = format_session_uri(local_addr.ip(), session_id);

        // 2) Timing must be live before encrypted Session SETUP.
        // Receivers advertising SupportsPTP use the native gPTP path first;
        // only an actual 319/320 start failure falls back to the NTP responder.
        let clock_id = dacp_clock_id(&config.dacp_id).ok_or_else(|| {
            NativeSessionError::Timing("DACP ID cannot form an 8-byte PTP clock id".into())
        })?;
        let device_id = dacp_device_id(&config.dacp_id).ok_or_else(|| {
            NativeSessionError::Timing("DACP ID cannot form deviceID".into())
        })?;
        let mac_address = dacp_mac_address(&config.dacp_id).ok_or_else(|| {
            NativeSessionError::Timing("DACP ID cannot form macAddress".into())
        })?;

        let mut ntp_timing = None;
        let mut ptp_timing = None;
        let event_port;

        if config.supports_ptp {
            match PtpEngine::start(
                receiver_ip,
                local_addr.ip(),
                clock_id,
                config.follow_receiver_clock,
            ) {
                Ok(engine) => {
                    // Match source settle window before publishing timingPeerInfo.
                    engine.settle(Duration::from_millis(400));
                    flow.timing_ready()
                        .map_err(|e| NativeSessionError::Flow(format!("{e:?}")))?;

                    let setup = PtpSessionSetupConfig {
                        cseq: 1,
                        session_uri: session_uri.clone(),
                        session_uuid: session_uuid.clone(),
                        group_uuid: random_uuid_upper(&mut rng),
                        peer_uuid: random_uuid_upper(&mut rng),
                        device_id,
                        mac_address,
                        name: config.receiver_name.clone(),
                        local_address: local_addr.ip().to_string(),
                        clock_id: engine.master_clock_id(),
                        dacp_id: config.dacp_id.clone(),
                        active_remote: config.active_remote.clone(),
                    };
                    let result = setup_ptp_session(&mut flow, &mut control, &setup)
                        .map_err(|e| NativeSessionError::SessionSetup(format!("{e:?}")))?;
                    event_port = result.event_port;
                    ptp_timing = Some(engine);
                }
                Err(_) => {
                    let timing_bind = SocketAddr::new(local_addr.ip(), 0);
                    let timing = start_ntp_timing_gate(&mut flow, timing_bind)
                        .map_err(|e| NativeSessionError::Timing(format!("{e:?}")))?;
                    let timing_port = timing
                        .port()
                        .map_err(NativeSessionError::LocalAddress)?;
                    let setup = NtpSessionSetupConfig {
                        cseq: 1,
                        session_uri: session_uri.clone(),
                        session_uuid: session_uuid.clone(),
                        device_id: Some(device_id),
                        timing_port,
                        dacp_id: config.dacp_id.clone(),
                        active_remote: config.active_remote.clone(),
                    };
                    let result = setup_ntp_session(&mut flow, &mut control, &setup)
                        .map_err(|e| NativeSessionError::SessionSetup(format!("{e:?}")))?;
                    event_port = result.event_port;
                    ntp_timing = Some(timing);
                }
            }
        } else {
            let timing_bind = SocketAddr::new(local_addr.ip(), 0);
            let timing = start_ntp_timing_gate(&mut flow, timing_bind)
                .map_err(|e| NativeSessionError::Timing(format!("{e:?}")))?;
            let timing_port = timing
                .port()
                .map_err(NativeSessionError::LocalAddress)?;
            let setup = NtpSessionSetupConfig {
                cseq: 1,
                session_uri: session_uri.clone(),
                session_uuid: session_uuid.clone(),
                device_id: Some(device_id),
                timing_port,
                dacp_id: config.dacp_id.clone(),
                active_remote: config.active_remote.clone(),
            };
            let result = setup_ntp_session(&mut flow, &mut control, &setup)
                .map_err(|e| NativeSessionError::SessionSetup(format!("{e:?}")))?;
            event_port = result.event_port;
            ntp_timing = Some(timing);
        }

        // 3) Keep-open reverse event TCP.
        let event = open_event_channel(
            &mut flow,
            receiver_ip,
            event_port,
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
            session_uri: session_uri.clone(),
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
            audio_secret,
            stream_connection_id: session_id,
        };
        let media = prepare_realtime_media(&mut flow, &mut control, &media)
            .map_err(|e| NativeSessionError::Media(format!("{e:?}")))?;

        // Upstream clamps the configured lead into the receiver-reported
        // latency window immediately after Stream SETUP.
        let requested_lead = config.lead_frames;
        let min_frames = media.latency_min.unwrap_or(0);
        let max_frames = media.latency_max.unwrap_or(requested_lead);
        let effective_lead_frames = if requested_lead < min_frames {
            min_frames
        } else if requested_lead > max_frames {
            max_frames
        } else {
            requested_lead
        };

        // 6) PTP only: upstream sends SETPEERS as the next RTSP exchange,
        // then hands the exact same [receiver, us] list to the timing engine and
        // kicks timing immediately. NTP sessions skip this exchange.
        let next_control_cseq = if let Some(engine) = ptp_timing.as_ref() {
            let setpeers = SetPeersConfig {
                cseq: 4,
                session_uri: session_uri.clone(),
                receiver_address: receiver_ip.to_string(),
                local_address: local_addr.ip().to_string(),
                dacp_id: config.dacp_id.clone(),
                active_remote: config.active_remote.clone(),
            };
            send_setpeers(&mut control, &setpeers)
                .map_err(|e| NativeSessionError::Media(format!("SETPEERS failed: {e:?}")))?;
            engine.set_peers(&[receiver_ip, local_addr.ip()]);
            5
        } else {
            4
        };

        // Only after Stream SETUP (+ SETPEERS for PTP) is the native transport Ready.
        flow.ready()
            .map_err(|e| NativeSessionError::Flow(format!("{e:?}")))?;

        // Match source: one encrypted RTSP channel, one global CSeq,
        // and a lock around each complete request/response exchange.
        let control = Arc::new(Mutex::new(control));
        let next_cseq = Arc::new(AtomicU32::new(next_control_cseq));

        // Match source ordering: an explicitly requested initial volume is
        // sent after native setup is complete but before the audio producer
        // starts. No configured volume means no SET_PARAMETER at all.
        let initial_volume_result = if let Some(volume) = config.initial_volume {
            Some(
                set_native_volume(
                    &control,
                    &next_cseq,
                    &session_uri,
                    &config.dacp_id,
                    &config.active_remote,
                    volume,
                )
                .map_err(|e| NativeSessionError::Volume(format!("{e:?}")))?,
            )
        } else {
            None
        };

        let feedback = FeedbackWorker::start(
            Arc::clone(&control),
            Arc::clone(&next_cseq),
            config.dacp_id.clone(),
            config.active_remote.clone(),
        )
        .map_err(NativeSessionError::Feedback)?;

        // Preserve the source clock-readiness floor; only the absolute START
        // instant is deferred until PCM is actually buffered.
        let mut cold_start_delay_ms = 250u64;
        if let Some(engine) = ptp_timing.as_ref() {
            if let Some(exchange) = engine.peer_exchange() {
                let readiness = clock_ready_delay_ms(exchange, config.apple_model);
                cold_start_delay_ms = cold_start_delay_ms.max(readiness);
            }
        }

        // Cold START is intentionally deferred until the Windows capture path
        // has one complete transport packet buffered. Upstream's caller gates
        // START on audio-present; sending/anchoring before that creates a
        // silence->content cold boundary that the reference path avoids.
        let pid = std::process::id();
        let rtp_offset = pid.wrapping_mul(2_654_435_761u32) & 0x0FFF_FF00u32;
        let sequence = pid.wrapping_mul(40_503u32) as u16;
        let rtp_timestamp = rtp_offset;

        let ptp_clock = ptp_timing.as_ref().map(PtpEngine::clock_handle);
        let ssrc = if ptp_clock.is_some() { 0 } else { session_id };
        let rtp = RtpState::new(sequence, rtp_timestamp, ssrc);

        // Realtime source always attempts the retransmit responder, but failure
        // is non-fatal: audio still runs, only packet repair is unavailable.
        let latency_max = media.latency_max;
        let rtx_ring = RetransmitRing::new();
        let retransmit = media
            .transport
            .clone_control_socket()
            .ok()
            .and_then(|socket| RetransmitWorker::start(socket, rtx_ring.clone()).ok());

        let mut sender = if let Some(clock) = ptp_clock.clone() {
            RealtimeMediaSender::new_ptp_clock(media.transport, rtp, audio_secret, clock)
        } else {
            RealtimeMediaSender::new(media.transport, rtp, audio_secret)
        };
        if retransmit.is_some() {
            sender.set_retransmit_ring(rtx_ring);
        }
        Ok(Self {
            flow,
            control,
            next_cseq,
            feedback,
            retransmit,
            _ntp_timing: ntp_timing,
            _ptp_timing: ptp_timing,
            event: Some(event),
            sender: Some(sender),
            session_uri: session_uri.clone(),
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
            lead_frames: effective_lead_frames,
            latency_max,
            rtp_offset,
            cold_start_delay_ms,
            initial_volume_result,
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

    pub fn control_channel(&self) -> crate::SharedRtspControl {
        Arc::clone(&self.control)
    }

    pub fn next_control_cseq(&self) -> u32 {
        self.next_cseq.load(Ordering::SeqCst)
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

        match WindowsAudioWorker::start(
            sender,
            self.lead_frames,
            self.latency_max,
            self.rtp_offset,
            self.cold_start_delay_ms,
        ) {
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
    pub fn audio_discontinuities(&self) -> u64 {
        self.audio_worker
            .as_ref()
            .map(WindowsAudioWorker::discontinuity_count)
            .unwrap_or(0)
    }

    #[cfg(windows)]
    pub fn audio_last_discontinuity_frame(&self) -> Option<u64> {
        self.audio_worker
            .as_ref()
            .and_then(WindowsAudioWorker::last_discontinuity_frame)
    }

    #[cfg(windows)]
    pub fn audio_first_non_silent_frame(&self) -> Option<u64> {
        self.audio_worker
            .as_ref()
            .and_then(WindowsAudioWorker::first_non_silent_frame)
    }

    pub fn retransmit_stats(&self) -> RetransmitStats {
        self.retransmit
            .as_ref()
            .map(RetransmitWorker::stats)
            .unwrap_or_default()
    }

    pub fn initial_volume_result(&self) -> Option<VolumeSetResult> {
        self.initial_volume_result
    }

    pub fn volume_control(&self) -> NativeVolumeControl {
        NativeVolumeControl::new(
            Arc::clone(&self.control),
            Arc::clone(&self.next_cseq),
            self.session_uri.clone(),
            self.dacp_id.clone(),
            self.active_remote.clone(),
        )
    }

    pub fn feedback_running(&self) -> bool {
        self.feedback.is_running()
    }

    pub fn feedback_error(&self) -> Option<String> {
        self.feedback.last_error()
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
        // Stop the realtime producer first, then the RTSP keepalive worker.
        // Only after both threads are joined may timing/event resources drop.
        #[cfg(windows)]
        self.stop_windows_audio();
        if let Some(worker) = self.retransmit.as_mut() {
            worker.stop();
        }
        self.feedback.stop();

        // Match upstream disconnect ordering: the reverse event channel is
        // closed before the final RTSP TEARDOWN, while timing remains alive.
        self.event.take();
        let _ = send_teardown(
            &self.control,
            &self.next_cseq,
            &self.session_uri,
            &self.dacp_id,
            &self.active_remote,
        );
    }
}

fn clock_ready_delay_ms(exchange: crate::PtpExchange, apple_model: bool) -> u64 {
    const CLOCK_LOCK_MS: u64 = 2300;
    const CLOCK_SETTLE_MS: u64 = 250;
    const CLOCK_SEAT_EXCHANGES: u32 = 3;

    let full = CLOCK_LOCK_MS.saturating_sub(exchange.first_ms);
    if apple_model && exchange.count >= CLOCK_SEAT_EXCHANGES {
        let fast = CLOCK_SETTLE_MS.saturating_sub(exchange.third_ms);
        full.min(fast)
    } else {
        full
    }
}

fn ms_to_ntp(ms: u64) -> u64 {
    ((ms as u128) << 32).div_ceil(1000) as u64
}

fn ntp_to_frames(ntp: u64, sample_rate: u64) -> u64 {
    let sec = ntp >> 32;
    let frac = ntp & 0xFFFF_FFFF;
    sec.saturating_mul(sample_rate)
        .saturating_add(((frac as u128 * sample_rate as u128) >> 32) as u64)
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

fn dacp_clock_id(dacp_id: &str) -> Option<u64> {
    let compact: String = dacp_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if compact.len() != 16 {
        return None;
    }
    u64::from_str_radix(&compact, 16).ok()
}

fn dacp_mac_address(dacp_id: &str) -> Option<String> {
    let compact: String = dacp_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if compact.len() != 16 {
        return None;
    }
    Some(
        (0..6)
            .map(|i| &compact[i * 2..i * 2 + 2])
            .collect::<Vec<_>>()
            .join(":")
            .to_ascii_uppercase(),
    )
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
    fn clock_readiness_matches_source_bounds() {
        let ex = crate::PtpExchange {
            count: 1,
            first_ms: 900,
            last_ms: 0,
            third_ms: 0,
        };
        assert_eq!(clock_ready_delay_ms(ex, false), 1400);
        assert_eq!(clock_ready_delay_ms(ex, true), 1400);

        let apple = crate::PtpExchange {
            count: 3,
            first_ms: 1200,
            last_ms: 0,
            third_ms: 100,
        };
        assert_eq!(clock_ready_delay_ms(apple, true), 150);
        assert_eq!(clock_ready_delay_ms(apple, false), 1100);
    }

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
