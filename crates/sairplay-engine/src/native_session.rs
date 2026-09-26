use crate::{
    open_event_channel, prepare_buffered_media, prepare_realtime_media, send_record, setup_ntp_session,
    send_setpeers, send_teardown, setup_ptp_session, start_ntp_timing_gate, Ap2PreflightClient,
    EventChannel, FeedbackWorker, MediaHandshakeConfig, NativeConnectFlow, NativePhase,
    NtpSessionSetupConfig, NtpTimingResponder, PairingError, PreflightError, PtpEngine,
    PtpSessionSetupConfig, RealtimeMediaSender, RecordConfig, RetransmitRing,
    Ap2AudioFormat, BufferedMediaSender, MediaTransport, NativeVolumeControl,
    ReceiverCapabilities, RetransmitStats, RetransmitWorker, Route, RouteResolver, RtpState,
    SetPeersConfig, NativeHapPairingClient, StoredHapCredentials, TransientPairingClient,
    VolumeSetResult, set_native_volume, system_time_to_ntp,
};
use rand::RngCore;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime};

const SOLO_COLD_START_LEAD_MS: u64 = 400;

#[cfg(windows)]
use crate::{NativeMetadataControl, WindowsAudioTarget, WindowsAudioWorker, WindowsAudioWorkerError};

#[derive(Debug, Clone)]
pub struct NativeSessionConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    /// Pinned MSA 192-hex HomeKit credentials; when present native AP2 uses pair-verify.
    pub auth_credentials: Option<String>,
    pub dacp_id: String,
    pub active_remote: String,
    pub lead_frames: u32,
    pub supports_ptp: bool,
    pub supports_buffered_audio: bool,
    /// Auto-select type 103 only when the caller has enabled this routing
    /// surface. Groups leave it off until mixed type96/type103 handoff lands.
    pub buffered_auto_enabled: bool,
    pub follow_receiver_clock: bool,
    pub apple_model: bool,
    pub receiver_name: String,
    pub initial_volume: Option<u8>,
    /// Mirrors Music Assistant's per-device 24-bit toggle. Capability alone
    /// never enables hi-res; callers opt in after applying their device-family
    /// default (HomePod is off by default upstream).
    pub hires_enabled: bool,
    /// Sample rate of the shared/source PCM session. Upstream keeps 44.1/48 kHz
    /// when supported and falls back to 44.1 kHz for any other source rate.
    pub session_sample_rate: u32,
}

impl NativeSessionConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            password: None,
            auth_credentials: None,
            dacp_id: "A1B2C3D4E5F60708".into(),
            active_remote: "123456789".into(),
            lead_frames: 11_025,
            supports_ptp: false,
            supports_buffered_audio: false,
            buffered_auto_enabled: false,
            follow_receiver_clock: false,
            apple_model: false,
            receiver_name: "SAirplay2 Receiver".into(),
            initial_volume: None,
            hires_enabled: false,
            session_sample_rate: 44_100,
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
    _ptp_timing: Option<Arc<PtpEngine>>,
    event: Option<EventChannel>,
    sender: Option<RealtimeMediaSender>,
    buffered_sender: Option<BufferedMediaSender>,
    _buffered_control_transport: Option<MediaTransport>,
    buffered_clock: Option<crate::PtpClock>,
    use_buffered: bool,
    session_uri: String,
    dacp_id: String,
    active_remote: String,
    lead_frames: u32,
    latency_max: Option<u32>,
    rtp_offset: u32,
    cold_start_delay_ms: u64,
    apple_model: bool,
    ptp_receiver_ip: Option<IpAddr>,
    initial_volume_result: Option<VolumeSetResult>,
    audio_format: Ap2AudioFormat,
    teardown_sent: bool,
    #[cfg(windows)]
    audio_worker: Option<WindowsAudioWorker>,
}

impl NativeSession {
    pub fn connect(config: &NativeSessionConfig) -> Result<Self, NativeSessionError> {
        Self::connect_with_shared_ptp(config, None)
    }

    pub fn connect_with_shared_ptp(
        config: &NativeSessionConfig,
        shared_ptp: Option<Arc<PtpEngine>>,
    ) -> Result<Self, NativeSessionError> {
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

        let local_addr = stream.local_addr().map_err(NativeSessionError::LocalAddress)?;
        let receiver_ip = info.peer.ip();

        // Pinned MSA auto policy is transport routing, not a format-union hint.
        // The GUI enables this only for Single sessions for now; native groups
        // stay realtime until the mixed type96/type103 worker is implemented.
        let buffered_auto_requested = config.buffered_auto_enabled
            && RouteResolver::buffered_auto_eligible(
                Route::AirPlay2Native,
                ReceiverCapabilities {
                    supports_airplay2: true,
                    supports_ptp: config.supports_ptp,
                    supports_buffered_audio: config.supports_buffered_audio,
                    is_apple_model: config.apple_model,
                    ..Default::default()
                },
            );

        // Match pinned Music Assistant: /info format tables decide whether
        // 24-bit is offered by taking the UNION of audioStream + bufferStream.
        // Once hi-res is enabled, cliairplay uses the requested bit depth/rate
        // directly and does not downgrade it from a per-stream table.
        let audio_format = if config.hires_enabled {
            if config.session_sample_rate >= 48_000 {
                Ap2AudioFormat::ALAC_48000_24_STEREO
            } else {
                Ap2AudioFormat::ALAC_44100_24_STEREO
            }
        } else {
            Ap2AudioFormat::ALAC_44100_16_STEREO
        };

        let pairing = if let Some(credentials_hex) = config.auth_credentials.as_deref() {
            let credentials = StoredHapCredentials::from_hex(credentials_hex)
                .map_err(NativeSessionError::Pairing)?;
            NativeHapPairingClient::default()
                .pair_verify_on_stream(stream, info.peer, &config.dacp_id, &credentials)
                .map_err(NativeSessionError::Pairing)?
        } else {
            TransientPairingClient::default()
                .pair_channel_on_stream(stream, info.peer, config.password.as_deref())
                .map_err(NativeSessionError::Pairing)?
        };

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
        let mut ptp_timing: Option<Arc<PtpEngine>> = None;
        let mut ptp_clock = None;
        let event_port;

        if config.supports_ptp {
            let engine_result = if let Some(engine) = shared_ptp.as_ref() {
                Ok(Arc::clone(engine))
            } else {
                PtpEngine::start(
                    receiver_ip,
                    local_addr.ip(),
                    clock_id,
                    config.follow_receiver_clock,
                )
                .map(Arc::new)
            };

            match engine_result {
                Ok(engine) => {
                    // airplay-cli v0.5.4 parity: one host-wide engine can
                    // follow a different receiver clock for each standalone
                    // HomePod while serving our grandmaster to other peers.
                    let clock = if shared_ptp.is_some() {
                        engine
                            .register_receiver(receiver_ip, config.follow_receiver_clock)
                            .map_err(|e| NativeSessionError::Timing(e.to_string()))?
                    } else {
                        engine
                            .clock_handle_for(receiver_ip)
                            .map_err(|e| NativeSessionError::Timing(e.to_string()))?
                    };
                    engine.settle_receiver(receiver_ip, Duration::from_millis(400));
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
                        clock_id: clock.master_clock_id(),
                        dacp_id: config.dacp_id.clone(),
                        active_remote: config.active_remote.clone(),
                    };
                    let result = setup_ptp_session(&mut flow, &mut control, &setup)
                        .map_err(|e| NativeSessionError::SessionSetup(format!("{e:?}")))?;
                    event_port = result.event_port;
                    ptp_clock = Some(clock);
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

        // Buffered type 103 is PTP-only upstream. If PTP startup falls back
        // to NTP, only the transport falls back to realtime type 96. The
        // requested audio format stays unchanged, matching cliairplay.
        let use_buffered = buffered_auto_requested && ptp_clock.is_some();

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

        // 5) Stream SETUP. Realtime advertises live UDP data/control ports;
        // buffered advertises only controlPort and then connects TCP to the
        // receiver-assigned dataPort.
        let media_config = MediaHandshakeConfig {
            bind_ip: local_addr.ip(),
            receiver_ip,
            cseq: 3,
            session_uri: session_uri.clone(),
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
            audio_secret,
            stream_connection_id: session_id,
            audio_format,
        };
        let mut realtime_media = None;
        let mut buffered_media = None;
        if use_buffered {
            buffered_media = Some(
                prepare_buffered_media(&mut flow, &mut control, &media_config)
                    .map_err(|e| NativeSessionError::Media(format!("{e:?}")))?,
            );
        } else {
            realtime_media = Some(
                prepare_realtime_media(&mut flow, &mut control, &media_config)
                    .map_err(|e| NativeSessionError::Media(format!("{e:?}")))?,
            );
        }

        // Realtime clamps configured lead to receiver latencyMin/Max. Buffered
        // omits those fields by design and schedules playback entirely by the
        // PTP rate anchor, so there is no receiver latency window to clamp.
        let requested_lead = ((config.lead_frames as u64
            * audio_format.sample_rate as u64)
            / 44_100) as u32;
        let latency_max = realtime_media.as_ref().and_then(|media| media.latency_max);
        let effective_lead_frames = if let Some(media) = realtime_media.as_ref() {
            let min_frames = media.latency_min.unwrap_or(0);
            let max_frames = media.latency_max.unwrap_or(requested_lead);
            if requested_lead < min_frames {
                min_frames
            } else if requested_lead > max_frames {
                max_frames
            } else {
                requested_lead
            }
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
            engine.add_peers(&[receiver_ip, local_addr.ip()]);
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

        // Match the pinned Music Assistant caller: a solo START is commanded
        // 400 ms ahead. The 250 ms value remains only the cliairplay minimum
        // warm/starvation floor; it is not the caller's solo START lead.
        // The absolute START instant is still deferred until PCM is buffered.
        let mut cold_start_delay_ms = SOLO_COLD_START_LEAD_MS;
        let mut clock_ready_at_ntp = None;
        if let Some(clock) = ptp_clock.as_ref() {
            if let Some(exchange) = clock.exchange() {
                let readiness = clock_ready_delay_ms(exchange, config.apple_model);
                cold_start_delay_ms = cold_start_delay_ms.max(readiness);
                if let Ok(now_ntp) = system_time_to_ntp(SystemTime::now()) {
                    clock_ready_at_ntp = Some(now_ntp.saturating_add(ms_to_ntp(readiness)));
                }
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

        let ssrc = if ptp_clock.is_some() { 0 } else { session_id };
        let rtp = RtpState::new(sequence, rtp_timestamp, ssrc);

        let mut retransmit = None;
        let mut sender = None;
        let mut buffered_sender = None;
        let mut buffered_control_transport = None;
        let mut buffered_clock = None;

        if use_buffered {
            let media = buffered_media
                .take()
                .expect("buffered media prepared for buffered route");
            buffered_sender = Some(BufferedMediaSender::new_with_format(
                media.data_stream,
                rtp,
                audio_secret,
                audio_format,
            ));
            buffered_control_transport = Some(media.control_transport);
            buffered_clock = ptp_clock.clone();
        } else {
            let media = realtime_media
                .take()
                .expect("realtime media prepared for realtime route");
            // Realtime source always attempts the retransmit responder, but
            // failure is non-fatal. Buffered type103 has no RTX path: TCP is
            // the reliability layer.
            let rtx_ring = RetransmitRing::new();
            retransmit = media
                .transport
                .clone_control_socket()
                .ok()
                .and_then(|socket| RetransmitWorker::start(socket, rtx_ring.clone()).ok());

            let mut realtime_sender = if let Some(clock) = ptp_clock.clone() {
                RealtimeMediaSender::new_ptp_clock_with_format(
                    media.transport,
                    rtp,
                    audio_secret,
                    clock,
                    audio_format,
                )
            } else {
                RealtimeMediaSender::new_with_format(
                    media.transport,
                    rtp,
                    audio_secret,
                    audio_format,
                )
            };
            if retransmit.is_some() {
                realtime_sender.set_retransmit_ring(rtx_ring);
            }
            sender = Some(realtime_sender);
        }
        let ptp_receiver_ip = ptp_timing.as_ref().map(|_| receiver_ip);
        Ok(Self {
            flow,
            control,
            next_cseq,
            feedback,
            retransmit,
            _ntp_timing: ntp_timing,
            _ptp_timing: ptp_timing,
            event: Some(event),
            sender,
            buffered_sender,
            _buffered_control_transport: buffered_control_transport,
            buffered_clock,
            use_buffered,
            session_uri: session_uri.clone(),
            dacp_id: config.dacp_id.clone(),
            active_remote: config.active_remote.clone(),
            lead_frames: effective_lead_frames,
            latency_max,
            rtp_offset,
            cold_start_delay_ms,
            apple_model: config.apple_model,
            ptp_receiver_ip,
            initial_volume_result,
            audio_format,
            teardown_sent: false,
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

    pub fn audio_format(&self) -> Ap2AudioFormat {
        self.audio_format
    }

    pub fn uses_buffered_audio(&self) -> bool {
        self.use_buffered
    }

    pub fn control_channel(&self) -> crate::SharedRtspControl {
        Arc::clone(&self.control)
    }

    pub fn next_control_cseq(&self) -> u32 {
        self.next_cseq.load(Ordering::SeqCst)
    }

    pub fn shared_ptp_engine(&self) -> Option<Arc<PtpEngine>> {
        self._ptp_timing.as_ref().map(Arc::clone)
    }

    #[cfg(windows)]
    pub fn take_windows_audio_target(
        &mut self,
        name: impl Into<String>,
    ) -> Result<WindowsAudioTarget, NativeSessionError> {
        if !self.is_ready() {
            return Err(NativeSessionError::Flow(
                "audio target cannot be extracted before native transport is Ready".into(),
            ));
        }
        if self.audio_worker.as_ref().is_some_and(|worker| worker.is_running()) {
            return Err(NativeSessionError::Flow(
                "audio target cannot be extracted while single-device capture is running".into(),
            ));
        }
        if self.use_buffered {
            return Err(NativeSessionError::Flow(
                "buffered type103 group handoff is not enabled until mixed type96/type103 MultiRoom support lands".into(),
            ));
        }
        let sender = self.sender.take().ok_or_else(|| {
            NativeSessionError::Flow("realtime sender is already owned by an audio worker".into())
        })?;
        Ok(WindowsAudioTarget {
            name: name.into(),
            sender,
            lead_frames: self.lead_frames,
            latency_max: self.latency_max,
            rtp_offset: self.rtp_offset,
            cold_start_delay_ms: self.cold_start_delay_ms,
            apple_model: self.apple_model,
            metadata: NativeMetadataControl::new(
                Arc::clone(&self.control),
                Arc::clone(&self.next_cseq),
                self.session_uri.clone(),
                self.dacp_id.clone(),
                self.active_remote.clone(),
            ),
        })
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

        let worker_result = if self.use_buffered {
            let sender = self.buffered_sender.take().ok_or_else(|| {
                NativeSessionError::Flow("buffered sender is already owned by audio worker".into())
            })?;
            let clock = self.buffered_clock.clone().ok_or_else(|| {
                NativeSessionError::Flow("buffered route is missing its PTP clock".into())
            })?;
            WindowsAudioWorker::start_buffered(
                sender,
                clock,
                Arc::clone(&self.control),
                Arc::clone(&self.next_cseq),
                self.session_uri.clone(),
                self.dacp_id.clone(),
                self.active_remote.clone(),
                self.cold_start_delay_ms,
            )
        } else {
            let sender = self.sender.take().ok_or_else(|| {
                NativeSessionError::Flow("realtime sender is already owned by audio worker".into())
            })?;
            WindowsAudioWorker::start(
                sender,
                self.lead_frames,
                self.latency_max,
                self.rtp_offset,
                self.cold_start_delay_ms,
                self.apple_model,
            )
        };

        match worker_result {
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

    #[cfg(windows)]
    pub fn drain_startup_events(&self) -> Vec<String> {
        self.audio_worker
            .as_ref()
            .map(WindowsAudioWorker::drain_startup_events)
            .unwrap_or_default()
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


    /// Source-aligned disconnect boundary: retire RTSP-side workers and send
    /// TEARDOWN while the realtime audio producer is still feeding the receiver.
    /// MSA explicitly keeps teardown ahead of starving the armed receiver queue.
    pub(crate) fn teardown_while_audio_hot(&mut self) {
        if self.teardown_sent {
            return;
        }
        if let Some(worker) = self.retransmit.as_mut() {
            worker.stop();
        }
        self.feedback.stop();
        self.event.take();
        let _ = send_teardown(
            &self.control,
            &self.next_cseq,
            &self.session_uri,
            &self.dacp_id,
            &self.active_remote,
        );
        self.teardown_sent = true;
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
        // MSA disconnect semantics: tell the receiver to tear down while its
        // realtime queue is still being fed. Starving an armed queue first can
        // produce an audible pop/noise burst on Apple receivers.
        self.teardown_while_audio_hot();

        #[cfg(windows)]
        self.stop_windows_audio();

        // Timing remains alive through TEARDOWN and is unregistered only after
        // the local audio producer has stopped.
        if let (Some(engine), Some(receiver_ip)) =
            (self._ptp_timing.as_ref(), self.ptp_receiver_ip)
        {
            engine.remove_receiver(receiver_ip);
        }
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
    fn solo_cold_start_lead_matches_pinned_music_assistant() {
        assert_eq!(SOLO_COLD_START_LEAD_MS, 400);
    }

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
