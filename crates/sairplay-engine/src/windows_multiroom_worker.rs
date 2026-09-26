use crate::{
    buffered_anchor_start, system_time_to_ntp, Ap2AudioFormat, BufferedAnchorStartConfig,
    BufferedMediaSender, BufferedWriteOutcome, NativeMetadataControl, Pcm352Chunker, PtpClock,
    RealtimeMediaSender, RtpState, SharedCseq, SharedRtspControl, WasapiLoopbackCapture,
    WasapiLoopbackError,
};
use std::collections::VecDeque;
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// Upstream Music Assistant constants, kept explicit so the Windows engine
/// follows the same synchronization policy.
pub const AIRPLAY_START_LEAD_MS: u64 = 400;
pub const AIRPLAY_COLD_GROUP_START_LEAD_MS: u64 = 2_500;
pub const AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS: u64 = 2_500;
pub const AIRPLAY_CLOCK_READY_TIMEOUT_MS: u64 = 2_500;
pub const AIRPLAY_CLOCK_READY_LEAD_MS: u64 = 500;
pub const AIRPLAY_SPLICE_LEAD_MARGIN_MS: u64 = 150;
pub const AIRPLAY_CLOCK_STALL_MS: u64 = 5_000;
pub const AIRPLAY_LATE_JOIN_RING_MIN_SECONDS: f64 = 12.0;
pub const AIRPLAY_LATE_JOIN_RING_MARGIN_SECONDS: f64 = 2.0;
pub const AIRPLAY_LATE_JOIN_RING_MAX_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsGroupAudioKind {
    StereoPair,
    MultiRoom,
}

impl WindowsGroupAudioKind {
    fn label(self) -> &'static str {
        match self {
            Self::StereoPair => "stereo-pair",
            Self::MultiRoom => "multi-room",
        }
    }
}

pub(crate) enum WindowsGroupMediaSender {
    Realtime(RealtimeMediaSender),
    Buffered {
        sender: BufferedMediaSender,
        clock: PtpClock,
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        session_uri: String,
        dacp_id: String,
        active_remote: String,
        ptp_observation_started: Instant,
        ptp_last_probe_observed: Option<Instant>,
    },
}

impl WindowsGroupMediaSender {
    pub(crate) fn realtime(sender: RealtimeMediaSender) -> Self {
        Self::Realtime(sender)
    }

    pub(crate) fn buffered(
        sender: BufferedMediaSender,
        clock: PtpClock,
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        session_uri: String,
        dacp_id: String,
        active_remote: String,
    ) -> Self {
        Self::Buffered {
            sender,
            clock,
            control,
            next_cseq,
            session_uri,
            dacp_id,
            active_remote,
            ptp_observation_started: Instant::now(),
            ptp_last_probe_observed: None,
        }
    }

    fn is_buffered(&self) -> bool {
        matches!(self, Self::Buffered { .. })
    }

    fn audio_format(&self) -> Ap2AudioFormat {
        match self {
            Self::Realtime(sender) => sender.audio_format(),
            Self::Buffered { sender, .. } => sender.audio_format(),
        }
    }

    fn state(&self) -> RtpState {
        match self {
            Self::Realtime(sender) => sender.state(),
            Self::Buffered { sender, .. } => sender.state(),
        }
    }

    fn pacing_window_frames(&self) -> u64 {
        match self {
            Self::Realtime(sender) => sender.pacing_window_frames(),
            Self::Buffered { sender, .. } => sender.pacing_window_frames(),
        }
    }

    fn uses_ptp_timing(&self) -> bool {
        match self {
            Self::Realtime(sender) => sender.uses_ptp_timing(),
            Self::Buffered { .. } => true,
        }
    }

    fn ptp_probe_exchange(&self) -> Option<crate::PtpExchange> {
        match self {
            Self::Realtime(sender) => sender.ptp_probe_exchange(),
            Self::Buffered { clock, .. } => clock.exchange(),
        }
    }

    fn observe_ptp_probe_exchange(&mut self) -> Option<crate::PtpExchange> {
        match self {
            Self::Realtime(sender) => sender.observe_ptp_probe_exchange(),
            Self::Buffered {
                clock,
                ptp_last_probe_observed,
                ..
            } => {
                let exchange = clock.exchange();
                if exchange.is_some() {
                    *ptp_last_probe_observed = Some(Instant::now());
                }
                exchange
            }
        }
    }

    fn ptp_probe_stalled(&self, stall_after: Duration) -> bool {
        match self {
            Self::Realtime(sender) => sender.ptp_probe_stalled(stall_after),
            Self::Buffered {
                clock,
                ptp_observation_started,
                ptp_last_probe_observed,
                ..
            } => {
                if clock.exchange().is_some() {
                    return false;
                }
                ptp_last_probe_observed
                    .unwrap_or(*ptp_observation_started)
                    .elapsed()
                    >= stall_after
            }
        }
    }

    fn arm_cold_start_verified(
        &mut self,
        requested_start_ntp: u64,
        latency_max: Option<u32>,
        lead_frames: u32,
        rtp_offset: u32,
        apple_model: bool,
    ) -> Result<u64, String> {
        match self {
            Self::Realtime(sender) => sender
                .arm_cold_start_verified(
                    requested_start_ntp,
                    latency_max,
                    lead_frames,
                    rtp_offset,
                    apple_model,
                )
                .map_err(|error| format!("{error:?}")),
            Self::Buffered {
                sender,
                clock,
                control,
                next_cseq,
                session_uri,
                dacp_id,
                active_remote,
                ..
            } => {
                // Type103 uses the same START feasibility contract, but its
                // timeline is committed with SETRATEANCHORTIME instead of a
                // realtime sync packet.
                let now_ntp = system_time_to_ntp(SystemTime::now())
                    .map_err(|error| format!("{error:?}"))?;
                let mut floor_ntp = now_ntp.saturating_add(ms_to_ntp(250));
                if let Some(exchange) = clock.exchange() {
                    floor_ntp = floor_ntp.max(
                        now_ntp.saturating_add(ms_to_ntp(clock_ready_delay_ms(
                            exchange,
                            apple_model,
                        ))),
                    );
                }
                let committed_start_ntp =
                    resolve_group_start_ntp(requested_start_ntp, floor_ntp);
                sender.arm_cold_start(committed_start_ntp);

                let config = BufferedAnchorStartConfig {
                    session_uri: session_uri.clone(),
                    dacp_id: dacp_id.clone(),
                    active_remote: active_remote.clone(),
                    rtp_time: sender.state().timestamp,
                    commanded_start_ntp: committed_start_ntp,
                };
                let mut channel = control
                    .lock()
                    .map_err(|_| "buffered RTSP control mutex poisoned".to_owned())?;
                buffered_anchor_start(&mut channel, next_cseq.as_ref(), clock, &config)
                    .map_err(|error| format!("{error:?}"))?;
                sender.mark_anchored();
                Ok(committed_start_ntp)
            }
        }
    }

    fn can_accept_frames(&mut self, now_ntp: u64) -> Result<bool, String> {
        match self {
            Self::Realtime(sender) => Ok(sender.can_accept_frames(now_ntp)),
            Self::Buffered { sender, .. } => sender
                .can_accept_frames(now_ntp)
                .map_err(|error| format!("{error:?}")),
        }
    }

    fn send_pcm_352(
        &mut self,
        packet: &[u8],
        now_ntp: u64,
        lead_frames: u32,
    ) -> Result<(), String> {
        match self {
            Self::Realtime(sender) => sender
                .send_pcm_352(packet, now_ntp, lead_frames)
                .map(|_| ())
                .map_err(|error| format!("{error:?}")),
            Self::Buffered { sender, .. } => sender
                .send_pcm_352(packet)
                .map(|outcome| match outcome {
                    BufferedWriteOutcome::Sent | BufferedWriteOutcome::Backpressured => (),
                })
                .map_err(|error| format!("{error:?}")),
        }
    }

    fn recover_delivery_gap(&mut self, now_ntp: u64, lead_frames: u32) -> Option<u32> {
        match self {
            Self::Realtime(sender) => sender.recover_delivery_gap(now_ntp, lead_frames),
            // Pinned cliairplay: buffered TCP needs no splice recovery; the
            // receiver owns its buffer and the RTP content line is continuous.
            Self::Buffered { .. } => None,
        }
    }

    fn recover_input_gap(&mut self, now_ntp: u64, lead_frames: u32) -> Option<u32> {
        match self {
            Self::Realtime(sender) => sender.recover_input_gap(now_ntp, lead_frames),
            Self::Buffered { .. } => None,
        }
    }

    fn splice_pad_frames(&self) -> u32 {
        match self {
            Self::Realtime(sender) => sender.splice_pad_frames(),
            Self::Buffered { .. } => 0,
        }
    }

    fn add_splice_pad(&mut self, frames: u32) {
        if let Self::Realtime(sender) = self {
            sender.add_splice_pad(frames);
        }
    }

    fn consume_splice_pad(&mut self, frames: u32) {
        if let Self::Realtime(sender) = self {
            sender.consume_splice_pad(frames);
        }
    }

    fn reanchor_shifted_frames(&self) -> u64 {
        match self {
            Self::Realtime(sender) => sender.reanchor_shifted_frames(),
            Self::Buffered { .. } => 0,
        }
    }
}

pub struct WindowsAudioTarget {
    pub(crate) name: String,
    pub(crate) sender: WindowsGroupMediaSender,
    pub(crate) lead_frames: u32,
    pub(crate) latency_max: Option<u32>,
    pub(crate) rtp_offset: u32,
    pub(crate) cold_start_delay_ms: u64,
    pub(crate) apple_model: bool,
    pub(crate) metadata: NativeMetadataControl,
}

#[derive(Debug)]
pub enum WindowsMultiroomAudioError {
    EmptyGroup,
    Capture(WasapiLoopbackError),
    Media(String),
    Time(String),
    Command(String),
}

impl fmt::Display for WindowsMultiroomAudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup => write!(f, "multi-room session has no audio targets"),
            Self::Capture(e) => write!(f, "{e}"),
            Self::Media(e) | Self::Time(e) | Self::Command(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WindowsMultiroomAudioError {}

enum GroupAudioCommand {
    Add {
        target: WindowsAudioTarget,
        reply: Sender<Result<(), String>>,
    },
    Remove {
        name: String,
        reply: Sender<Result<(), String>>,
    },
}

struct PendingJoin {
    target: WindowsAudioTarget,
    queued_packets: VecDeque<Vec<u8>>,
    anchor_packet: u64,
    skip_packets: u64,
    ready_wait_started: Instant,
    armed: bool,
    reply: Sender<Result<(), String>>,
}

#[derive(Clone)]
pub struct WindowsMultiroomJoinHandle {
    command_tx: Sender<GroupAudioCommand>,
}

impl WindowsMultiroomJoinHandle {
    pub fn add_target(&self, target: WindowsAudioTarget) -> Result<(), WindowsMultiroomAudioError> {
        let name = target.name.clone();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.command_tx
            .send(GroupAudioCommand::Add {
                target,
                reply: reply_tx,
            })
            .map_err(|_| {
                WindowsMultiroomAudioError::Command(
                    "multi-room audio worker is not accepting new members".into(),
                )
            })?;

        // Pinned Music Assistant allows up to 35 seconds for a late joiner's
        // priming write. A 24-bit/48 kHz receiver can legitimately need more
        // than 12 seconds of retained PCM to catch the shared live head, so a
        // shorter local timeout can reject a healthy join after START/PTP have
        // already succeeded.
        match reply_rx.recv_timeout(Duration::from_secs(35)) {
            Ok(result) => result.map_err(WindowsMultiroomAudioError::Command),
            Err(error) => {
                // Do not leave an orphaned pending/active target behind after
                // the caller gives up. Removal covers both races: still pending
                // in the prime path or attached just as the timeout fires.
                let (remove_tx, remove_rx) = mpsc::channel();
                let _ = self.command_tx.send(GroupAudioCommand::Remove {
                    name,
                    reply: remove_tx,
                });
                let _ = remove_rx.recv_timeout(Duration::from_secs(3));
                Err(WindowsMultiroomAudioError::Command(format!(
                    "late join timed out waiting for the shared timeline: {error}"
                )))
            }
        }
    }

    pub fn remove_target(&self, name: impl Into<String>) -> Result<(), WindowsMultiroomAudioError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.command_tx
            .send(GroupAudioCommand::Remove {
                name: name.into(),
                reply: reply_tx,
            })
            .map_err(|_| {
                WindowsMultiroomAudioError::Command(
                    "multi-room audio worker is not accepting membership changes".into(),
                )
            })?;
        reply_rx
            .recv_timeout(Duration::from_secs(3))
            .map_err(|error| {
                WindowsMultiroomAudioError::Command(format!(
                    "member removal timed out: {error}"
                ))
            })?
            .map_err(WindowsMultiroomAudioError::Command)
    }
}

pub struct WindowsMultiroomAudioWorker {
    kind: WindowsGroupAudioKind,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
    first_non_silent_frame: Arc<AtomicU64>,
    startup_events: Arc<Mutex<Vec<String>>>,
    failed_members: Arc<Mutex<Vec<(String, String)>>>,
    active_members: Arc<AtomicU64>,
    join_handle: WindowsMultiroomJoinHandle,
}

impl WindowsMultiroomAudioWorker {
    pub fn start(kind: WindowsGroupAudioKind, mut targets: Vec<WindowsAudioTarget>) -> Result<Self, WindowsMultiroomAudioError> {
        if targets.is_empty() {
            return Err(WindowsMultiroomAudioError::EmptyGroup);
        }
        match kind {
            WindowsGroupAudioKind::StereoPair if targets.len() != 2 => return Err(WindowsMultiroomAudioError::Media(format!("stereo-pair worker requires exactly 2 targets, got {}", targets.len()))),
            WindowsGroupAudioKind::MultiRoom if targets.len() < 2 => return Err(WindowsMultiroomAudioError::Media(format!("multi-room worker requires at least 2 targets, got {}", targets.len()))),
            _ => {}
        }

        let sample_rate = targets[0].sender.audio_format().sample_rate;
        if targets
            .iter()
            .any(|target| target.sender.audio_format().sample_rate != sample_rate)
        {
            return Err(WindowsMultiroomAudioError::Media(
                "multi-room per-member sample-rate conversion is not available yet".into(),
            ));
        }

        // Upstream AirPlay groups own one shared source PCM stream and then
        // convert per member before each cliairplay stdin. Mirror that shape:
        // capture the richest bit depth required by the current group, then
        // derive each member's exact handoff format independently.
        let source_format = if targets
            .iter()
            .any(|target| target.sender.audio_format().bit_depth > 16)
        {
            if sample_rate == 48_000 {
                Ap2AudioFormat::ALAC_48000_24_STEREO
            } else {
                Ap2AudioFormat::ALAC_44100_24_STEREO
            }
        } else if sample_rate == 48_000 {
            Ap2AudioFormat::ALAC_48000_16_STEREO
        } else {
            Ap2AudioFormat::ALAC_44100_16_STEREO
        };
        let bytes_per_frame = source_format.input_bytes_per_frame();

        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let last_error = Arc::new(Mutex::new(None));
        let last_error_thread = Arc::clone(&last_error);
        let discontinuities = Arc::new(AtomicU64::new(0));
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let last_discontinuity_frame_thread = Arc::clone(&last_discontinuity_frame);
        let first_non_silent_frame = Arc::new(AtomicU64::new(u64::MAX));
        let first_non_silent_frame_thread = Arc::clone(&first_non_silent_frame);
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));
        let startup_events_thread = Arc::clone(&startup_events);
        let failed_members = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let failed_members_thread = Arc::clone(&failed_members);
        let active_members = Arc::new(AtomicU64::new(targets.len() as u64));
        let active_members_thread = Arc::clone(&active_members);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (command_tx, command_rx) = mpsc::channel::<GroupAudioCommand>();
        let join_handle = WindowsMultiroomJoinHandle {
            command_tx: command_tx.clone(),
        };

        let worker_name = match kind {
            WindowsGroupAudioKind::StereoPair => "sairplay-stereo-pair-audio",
            WindowsGroupAudioKind::MultiRoom => "sairplay-multiroom-audio",
        };
        let worker = thread::Builder::new()
            .name(worker_name.into())
            .spawn(move || {
                let capture = match WasapiLoopbackCapture::open_default_for_format(source_format) {
                    Ok(capture) => {
                        let _ = ready_tx.send(Ok(()));
                        capture
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some(message);
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let mut chunker = Pcm352Chunker::new_with_bytes_per_frame(bytes_per_frame);
                let mut captured_frames_total = 0u64;
                let mut cold_armed = false;
                let mut group_start_ntp: Option<u64> = None;
                let mut input_starved_since: Option<Instant> = None;
                let mut packet_index = 0u64;
                let mut pending_joins = Vec::<PendingJoin>::new();
                let mut late_join_ring = VecDeque::<(u64, Vec<u8>)>::new();

                while running_thread.load(Ordering::SeqCst) {
                    handle_group_commands(
                        &command_rx,
                        &mut targets,
                        &mut pending_joins,
                        cold_armed,
                        &active_members_thread,
                        &startup_events_thread,
                        source_format,
                    );

                    // MSA waits for a late joiner's receiver-clock result outside
                    // the live-session lock. Do the native equivalent here:
                    // existing members keep flowing while an unarmed joiner
                    // waits for projection/timeout, and only then is its exact
                    // group-timeline instant committed.
                    prepare_pending_joins(
                        &mut pending_joins,
                        &targets,
                        group_start_ntp,
                        packet_index,
                        &startup_events_thread,
                        source_format,
                        &late_join_ring,
                    );

                    let report = match capture.drain_into(&mut chunker) {
                        Ok(report) => report,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(error.to_string());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    };

                    if report.discontinuities != 0 {
                        discontinuities_thread.fetch_add(report.discontinuities, Ordering::SeqCst);
                        if let Some(offset) = report.discontinuity_frame_offset {
                            last_discontinuity_frame_thread.store(
                                captured_frames_total.saturating_add(offset),
                                Ordering::SeqCst,
                            );
                        }
                    }
                    if let Some(offset) = report.first_non_silent_frame_offset {
                        let absolute = captured_frames_total.saturating_add(offset);
                        let _ = first_non_silent_frame_thread.compare_exchange(
                            u64::MAX,
                            absolute,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                    }
                    let frames = report.frames;
                    captured_frames_total = captured_frames_total.saturating_add(frames as u64);

                    // Match MSA: PCM amplitude never defines stream state.
                    // Any captured PCM bytes, including digital-zero samples, are
                    // valid input. Cold START is gated only by one complete 352-frame
                    // transport packet being buffered below.
                    if !cold_armed {
                        if !chunker.has_packet() {
                            if frames == 0 {
                                thread::sleep(Duration::from_millis(1));
                            }
                            continue;
                        }
                        let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                            Ok(value) => value,
                            Err(error) => {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };
                        let source_lead = if targets.len() > 1 {
                            AIRPLAY_COLD_GROUP_START_LEAD_MS
                        } else {
                            AIRPLAY_START_LEAD_MS
                        };
                        let clock_projections = wait_members_clock_projections_ms(
                            &targets,
                            Duration::from_millis(AIRPLAY_CLOCK_READY_TIMEOUT_MS),
                        );
                        let clock_projection_ms = clock_projections
                            .iter()
                            .filter_map(|(_, projection)| *projection)
                            .max();
                        let readiness_lead_ms = clock_projection_ms
                            .map(|delay| delay.saturating_add(AIRPLAY_CLOCK_READY_LEAD_MS))
                            .unwrap_or(0);
                        // MSA anchors at max(now + cold-group lead,
                        // latest projected receiver-ready instant + 500 ms).
                        // A receiver that reports no projection contributes
                        // nothing and rides the ordinary start lead.
                        let delay_ms = source_lead.max(readiness_lead_ms);
                        let start_ntp = now_ntp.saturating_add(ms_to_ntp(delay_ms));
                        if let Ok(mut events) = startup_events_thread.lock() {
                            for (name, projection) in &clock_projections {
                                events.push(match projection {
                                    Some(delay) => format!(
                                        "AirPlay group clock member: {} · projection={} ms.",
                                        name, delay
                                    ),
                                    None => format!(
                                        "AirPlay group clock member: {} · no PTP projection within {} ms.",
                                        name, AIRPLAY_CLOCK_READY_TIMEOUT_MS
                                    ),
                                });
                            }
                            events.push(match clock_projection_ms {
                                Some(delay) => format!(
                                    "AirPlay group clock readiness: latest projection in {} ms; shared START lead={} ms.",
                                    delay, delay_ms
                                ),
                                None => format!(
                                    "AirPlay group clock readiness: no PTP projection reported; shared START lead={} ms.",
                                    delay_ms
                                ),
                            });
                        }

                        // MSA does not trust the requested START blindly.
                        // Every member returns the instant it actually committed;
                        // if any member corrected forward, all members are
                        // re-STARTed on the largest reported instant (+150 ms
                        // command fan-out margin), for at most four rounds.
                        let mut requested_start_ntp = start_ntp;
                        let mut committed_group_ntp = start_ntp;
                        let mut converged = false;
                        let mut rounds = 0usize;

                        for round in 1..=4 {
                            rounds = round;
                            let mut latest_committed_ntp = requested_start_ntp;
                            for target in &mut targets {
                                let committed = match target.sender.arm_cold_start_verified(
                                    requested_start_ntp,
                                    target.latency_max,
                                    target.lead_frames,
                                    target.rtp_offset,
                                    target.apple_model,
                                ) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        if let Ok(mut slot) = last_error_thread.lock() {
                                            *slot = Some(format!(
                                                "{} cold session START failed: {error:?}",
                                                target.name
                                            ));
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                latest_committed_ntp = latest_committed_ntp.max(committed);
                            }

                            let correction_ntp =
                                latest_committed_ntp.saturating_sub(requested_start_ntp);
                            if correction_ntp <= ms_to_ntp(2) {
                                committed_group_ntp = requested_start_ntp;
                                converged = true;
                                break;
                            }

                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "AirPlay group START corrected: round {round}/4 · member floor moved shared instant +{} ms.",
                                    ntp_delta_to_ms(correction_ntp)
                                ));
                            }
                            committed_group_ntp = latest_committed_ntp;
                            if round < 4 {
                                requested_start_ntp = latest_committed_ntp
                                    .saturating_add(ms_to_ntp(AIRPLAY_SPLICE_LEAD_MARGIN_MS));
                            }
                        }

                        if !converged {
                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "AirPlay group START did not converge after 4 rounds; latest committed instant retained for diagnostics."
                                ));
                            }
                        }

                        for target in &mut targets {
                            let rtp_timestamp = target.sender.state().timestamp;
                            match target.metadata.send("SAirplay2", "", "", rtp_timestamp) {
                                Ok(result) if (200..300).contains(&result.status) => {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "{}: initial DMAP metadata {} bytes · RTSP {}.",
                                            target.name, result.bytes, result.status
                                        ));
                                    }
                                }
                                Ok(result) => {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "{}: initial DMAP metadata rejected · RTSP {}.",
                                            target.name, result.status
                                        ));
                                    }
                                }
                                Err(error) => {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "{}: initial DMAP metadata failed: {error:?}.",
                                            target.name
                                        ));
                                    }
                                }
                            }
                        }
                        cold_armed = true;
                        group_start_ntp = Some(committed_group_ntp);
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(format!(
                                "AirPlay {} session: {} member(s) ready · shared START={} ms · verified in {} round(s) · one WASAPI source.",
                                kind.label(),
                                targets.len(),
                                ntp_delta_to_ms(committed_group_ntp.saturating_sub(now_ntp)),
                                rounds
                            ));
                        }
                    }

                    let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    };

                    if chunker.has_packet() {
                        input_starved_since = None;
                        for target in &mut targets {
                            let _ = target
                                .sender
                                .recover_delivery_gap(now_ntp, target.lead_frames);
                        }
                        align_splice_pad(&mut targets);
                    } else if frames > 0 {
                        input_starved_since = None;
                    } else {
                        let started = input_starved_since.get_or_insert_with(Instant::now);
                        if started.elapsed() >= Duration::from_millis(250) {
                            for target in &mut targets {
                                let _ = target
                                    .sender
                                    .recover_input_gap(now_ntp, target.lead_frames);
                            }
                            align_splice_pad(&mut targets);
                            input_starved_since = Some(Instant::now());
                        }
                    }



                    // WASAPI loopback has no EOF sentinel. Match cliairplay:
                    // frames==0 while PLAYING is starvation (handled above);
                    // do not synthesize EOF/idle from PCM amplitude and do not
                    // start a separate silence-keepalive state here.

                    loop {
                        if targets.is_empty() && pending_joins.is_empty() {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some("all AirPlay session members stopped".into());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }

                        if targets.is_empty() {
                            break;
                        }

                        align_splice_pad(&mut targets);
                        let pad_now = targets
                            .iter()
                            .map(|target| target.sender.splice_pad_frames())
                            .max()
                            .unwrap_or(0)
                            .min(352);
                        let real_frames_needed = 352usize - pad_now as usize;
                        let real_bytes_needed = real_frames_needed * bytes_per_frame;
                        if chunker.pending_bytes() < real_bytes_needed {
                            break;
                        }

                        let ntp = match system_time_to_ntp(SystemTime::now()) {
                            Ok(value) => value,
                            Err(error) => {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };

                        let gate_index = targets
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, target)| target.sender.pacing_window_frames())
                            .map(|(index, _)| index)
                            .unwrap_or(0);
                        let gate_lead = targets[gate_index].lead_frames;

                        // MSA fans one PCM chunk to every member and waits for
                        // all writes before advancing the shared source. With a
                        // buffered TCP member, its parked tail/backpressure must
                        // therefore hold the NEXT group packet rather than let
                        // realtime members run ahead by a sample block.
                        let mut every_member_ready = true;
                        let mut gate_failed = Vec::<(usize, String)>::new();
                        for (index, target) in targets.iter_mut().enumerate() {
                            match target.sender.can_accept_frames(ntp) {
                                Ok(true) => {}
                                Ok(false) => every_member_ready = false,
                                Err(error) => gate_failed.push((
                                    index,
                                    format!("{} pacing/data channel failed: {error}", target.name),
                                )),
                            }
                        }
                        if !gate_failed.is_empty() {
                            for (index, message) in gate_failed.into_iter().rev() {
                                let name = targets
                                    .get(index)
                                    .map(|target| target.name.clone())
                                    .unwrap_or_else(|| "unknown".to_owned());
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!("AirPlay member removed: {message}"));
                                }
                                if let Ok(mut failed_members) = failed_members_thread.lock() {
                                    failed_members.push((name, message.clone()));
                                }
                                targets.remove(index);
                            }
                            active_members_thread.store(targets.len() as u64, Ordering::SeqCst);
                            continue;
                        }
                        if !every_member_ready {
                            thread::sleep(Duration::from_millis(1));
                            break;
                        }

                        let packet = chunker
                            .pop_packet_with_silence_prefix(pad_now)
                            .expect("group real-byte count checked");
                        let mut failed = Vec::<(usize, String)>::new();

                        for (index, target) in targets.iter_mut().enumerate() {
                            let target_format = target.sender.audio_format();
                            let target_packet = match adapt_group_pcm_packet(
                                &packet,
                                source_format,
                                target_format,
                            ) {
                                Ok(packet) => packet,
                                Err(error) => {
                                    failed.push((index, format!("{} media format failed: {error}", target.name)));
                                    continue;
                                }
                            };
                            match target.sender.send_pcm_352(
                                &target_packet,
                                ntp,
                                target.lead_frames,
                            ) {
                                Ok(_) => {}
                                Err(error) => failed.push((
                                    index,
                                    format!("{} media send failed: {error:?}", target.name),
                                )),
                            }
                        }
                        let sent_packet_index = packet_index;
                        packet_index = packet_index.saturating_add(1);

                        // MSA keeps one shared raw-PCM ring for late joiners.
                        // Grow it to cover the measured write-head lead + margin,
                        // with the same 12 s floor and 6 MiB hard cap.
                        late_join_ring.push_back((sent_packet_index, packet.clone()));
                        trim_late_join_ring(
                            &mut late_join_ring,
                            source_format,
                            group_start_ntp,
                            packet_index,
                            targets.first().map(|target| target.sender.reanchor_shifted_frames()).unwrap_or(0),
                        );

                        // Feed each late joiner from its mapped historical packet
                        // through the current write head while the existing group
                        // keeps playing. Once its queue catches the live head,
                        // attach it to normal fan-out on the next packet.
                        let mut join_index = 0usize;
                        while join_index < pending_joins.len() {
                            if !pending_joins[join_index].armed {
                                join_index += 1;
                                continue;
                            }
                            // A committed join instant may lie ahead of the
                            // current write head. Match MSA's live-feed skip:
                            // discard whole shared PCM packets until the source
                            // packet mapped to that instant reaches the head.
                            if pending_joins[join_index].skip_packets > 0 {
                                pending_joins[join_index].skip_packets -= 1;
                                join_index += 1;
                                continue;
                            }
                            pending_joins[join_index]
                                .queued_packets
                                .push_back(packet.clone());

                            let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                                Ok(value) => value,
                                Err(_) => {
                                    join_index += 1;
                                    continue;
                                }
                            };

                            let mut join_failed = None::<String>;
                            loop {
                                let can_send = {
                                    let pending = &mut pending_joins[join_index];
                                    if pending.queued_packets.is_empty() {
                                        Ok(false)
                                    } else {
                                        pending.target.sender.can_accept_frames(now_ntp)
                                    }
                                };
                                match can_send {
                                    Ok(true) => {}
                                    Ok(false) => break,
                                    Err(error) => {
                                        join_failed = Some(format!(
                                            "{} late-join pacing/data channel failed: {error}",
                                            pending_joins[join_index].target.name
                                        ));
                                        break;
                                    }
                                }

                                let source_packet = pending_joins[join_index]
                                    .queued_packets
                                    .front()
                                    .cloned()
                                    .expect("late-join queue checked");
                                let target_format =
                                    pending_joins[join_index].target.sender.audio_format();
                                let target_packet = match adapt_group_pcm_packet(
                                    &source_packet,
                                    source_format,
                                    target_format,
                                ) {
                                    Ok(packet) => packet,
                                    Err(error) => {
                                        join_failed = Some(format!(
                                            "{} late-join media format failed: {error}",
                                            pending_joins[join_index].target.name
                                        ));
                                        break;
                                    }
                                };
                                let lead_frames = pending_joins[join_index].target.lead_frames;
                                match pending_joins[join_index]
                                    .target
                                    .sender
                                    .send_pcm_352(&target_packet, now_ntp, lead_frames)
                                {
                                    Ok(_) => {
                                        pending_joins[join_index].queued_packets.pop_front();
                                    }
                                    Err(error) => {
                                        join_failed = Some(format!(
                                            "{} late-join media send failed: {error:?}",
                                            pending_joins[join_index].target.name
                                        ));
                                        break;
                                    }
                                }
                            }

                            if let Some(error) = join_failed {
                                let pending = pending_joins.remove(join_index);
                                let _ = pending.reply.send(Err(error.clone()));
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(error);
                                }
                                continue;
                            }

                            if pending_joins[join_index].queued_packets.is_empty() {
                                let pending = pending_joins.remove(join_index);
                                let name = pending.target.name.clone();
                                let anchor_packet = pending.anchor_packet;
                                targets.push(pending.target);
                                active_members_thread.store(targets.len() as u64, Ordering::SeqCst);
                                let _ = pending.reply.send(Ok(()));
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "Late joiner {name}: primed from packet #{anchor_packet} and caught the live head at packet #{}.",
                                        packet_index.saturating_sub(1)
                                    ));
                                }
                            } else {
                                join_index += 1;
                            }
                        }

                        for target in &mut targets {
                            target.sender.consume_splice_pad(pad_now);
                        }

                        if !failed.is_empty() {
                            for (index, message) in failed.into_iter().rev() {
                                let name = targets
                                    .get(index)
                                    .map(|target| target.name.clone())
                                    .unwrap_or_else(|| "unknown".to_owned());
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!("AirPlay member removed: {message}"));
                                }
                                if let Ok(mut failed_members) = failed_members_thread.lock() {
                                    failed_members.push((name, message.clone()));
                                }
                                targets.remove(index);
                            }
                            active_members_thread.store(targets.len() as u64, Ordering::SeqCst);
                        }

                        if packet_index <= 8 {
                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "AirPlay session: packet #{} fan-out to {} member(s) · gate_lead_frames={} · pad={}.",
                                    packet_index,
                                    targets.len(),
                                    gate_lead,
                                    pad_now
                                ));
                            }
                        }
                    }

                    if frames == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }

                for pending in pending_joins {
                    let _ = pending.reply.send(Err(
                        "multi-room audio worker stopped before late join completed".into(),
                    ));
                }
            })
            .map_err(|e| {
                WindowsMultiroomAudioError::Capture(WasapiLoopbackError::Windows(format!(
                    "failed to spawn MultiRoom WASAPI worker: {e}"
                )))
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                kind,
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
                first_non_silent_frame,
                startup_events,
                failed_members,
                active_members,
                join_handle,
            }),
            Ok(Err(message)) => {
                let _ = worker.join();
                Err(WindowsMultiroomAudioError::Capture(
                    WasapiLoopbackError::Windows(message),
                ))
            }
            Err(error) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(WindowsMultiroomAudioError::Capture(
                    WasapiLoopbackError::Windows(format!(
                        "MultiRoom WASAPI worker startup timeout: {error}"
                    )),
                ))
            }
        }
    }

    pub fn kind(&self) -> WindowsGroupAudioKind {
        self.kind
    }

    pub fn join_handle(&self) -> WindowsMultiroomJoinHandle {
        self.join_handle.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn active_members(&self) -> usize {
        self.active_members.load(Ordering::SeqCst) as usize
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn discontinuity_count(&self) -> u64 {
        self.discontinuities.load(Ordering::SeqCst)
    }

    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        match self.last_discontinuity_frame.load(Ordering::SeqCst) {
            u64::MAX => None,
            value => Some(value),
        }
    }

    pub fn first_non_silent_frame(&self) -> Option<u64> {
        match self.first_non_silent_frame.load(Ordering::SeqCst) {
            u64::MAX => None,
            value => Some(value),
        }
    }

    pub fn drain_startup_events(&self) -> Vec<String> {
        self.startup_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    pub fn drain_failed_members(&self) -> Vec<(String, String)> {
        self.failed_members
            .lock()
            .map(|mut failed| std::mem::take(&mut *failed))
            .unwrap_or_default()
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WindowsMultiroomAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn adapt_group_pcm_packet(
    packet: &[u8],
    source: Ap2AudioFormat,
    target: Ap2AudioFormat,
) -> Result<Vec<u8>, String> {
    if source.sample_rate != target.sample_rate || source.channels != target.channels {
        return Err("sample-rate/channel conversion is not available".into());
    }
    if source.bit_depth == target.bit_depth {
        return Ok(packet.to_vec());
    }

    // The upstream 24-bit handoff is s32le with the useful sample in the upper
    // 24 bits. A 16-bit member receives the upper 16 bits of that same sample,
    // exactly the per-member depth reduction needed before its 16-bit ALAC path.
    if source.bit_depth > 16 && target.bit_depth == 16 {
        if packet.len() % 4 != 0 {
            return Err("misaligned s32le shared PCM".into());
        }
        let mut out = Vec::with_capacity(packet.len() / 2);
        for sample in packet.chunks_exact(4) {
            out.extend_from_slice(&sample[2..4]);
        }
        return Ok(out);
    }

    Err("unsupported shared PCM conversion".into())
}

fn handle_group_commands(
    command_rx: &Receiver<GroupAudioCommand>,
    targets: &mut Vec<WindowsAudioTarget>,
    pending_joins: &mut Vec<PendingJoin>,
    cold_armed: bool,
    active_members: &AtomicU64,
    startup_events: &Mutex<Vec<String>>,
    source_format: Ap2AudioFormat,
) {
    while let Ok(command) = command_rx.try_recv() {
        match command {
            GroupAudioCommand::Add { target, reply } => {
                let target_format = target.sender.audio_format();
                if target_format.sample_rate != source_format.sample_rate
                    || (target_format.bit_depth > source_format.bit_depth)
                {
                    let _ = reply.send(Err(format!(
                        "{} requires {}-bit/{} Hz but the live shared source is {}-bit/{} Hz; restart the group so the shared source can be renegotiated",
                        target.name,
                        target_format.bit_depth,
                        target_format.sample_rate,
                        source_format.bit_depth,
                        source_format.sample_rate,
                    )));
                    continue;
                }

                if targets.iter().any(|item| item.name == target.name)
                    || pending_joins.iter().any(|item| item.target.name == target.name)
                {
                    let _ = reply.send(Err(format!(
                        "{} is already in the AirPlay session",
                        target.name
                    )));
                    continue;
                }

                if !cold_armed {
                    let name = target.name.clone();
                    targets.push(target);
                    active_members.store(targets.len() as u64, Ordering::SeqCst);
                    let _ = reply.send(Ok(()));
                    if let Ok(mut events) = startup_events.lock() {
                        events.push(format!("{name}: joined before the initial shared START."));
                    }
                    continue;
                }

                let name = target.name.clone();
                let timing = if target.sender.uses_ptp_timing() { "PTP" } else { "NTP" };
                pending_joins.push(PendingJoin {
                    target,
                    queued_packets: VecDeque::new(),
                    anchor_packet: 0,
                    skip_packets: 0,
                    ready_wait_started: Instant::now(),
                    armed: false,
                    reply,
                });
                if let Ok(mut events) = startup_events.lock() {
                    events.push(format!(
                        "{name}: late join connected on {timing}; waiting for receiver-clock readiness before committing its group instant."
                    ));
                }
            }
            GroupAudioCommand::Remove { name, reply } => {
                if let Some(index) = pending_joins
                    .iter()
                    .position(|item| item.target.name == name)
                {
                    let pending = pending_joins.remove(index);
                    let _ = pending
                        .reply
                        .send(Err(format!("{name}: late join cancelled by removal")));
                    let _ = reply.send(Ok(()));
                    continue;
                }

                if let Some(index) = targets.iter().position(|item| item.name == name) {
                    targets.remove(index);
                    active_members.store(targets.len() as u64, Ordering::SeqCst);
                    let _ = reply.send(Ok(()));
                    if let Ok(mut events) = startup_events.lock() {
                        events.push(format!("{name}: removed from live AirPlay session."));
                    }
                } else {
                    let _ = reply.send(Err(format!("{name} is not in the AirPlay session")));
                }
            }
        }
    }
}

fn prepare_pending_joins(
    pending_joins: &mut Vec<PendingJoin>,
    targets: &[WindowsAudioTarget],
    group_start_ntp: Option<u64>,
    packet_index: u64,
    startup_events: &Mutex<Vec<String>>,
    source_format: Ap2AudioFormat,
    late_join_ring: &VecDeque<(u64, Vec<u8>)>,
) {
    let Some(base_start) = group_start_ntp else {
        return;
    };

    let shift_frames = targets
        .first()
        .map(|item| item.sender.reanchor_shifted_frames())
        .unwrap_or(0);
    let sample_rate = source_format.sample_rate;
    let effective_start =
        base_start.saturating_add(frames_to_ntp(shift_frames, sample_rate));
    let oldest_ring_packet = late_join_ring
        .front()
        .map(|(index, _)| *index)
        .unwrap_or(packet_index);

    let mut index = 0usize;
    while index < pending_joins.len() {
        if pending_joins[index].armed {
            index += 1;
            continue;
        }

        let uses_ptp = pending_joins[index].target.sender.uses_ptp_timing();
        let mut readiness = "not-applicable";
        let mut projection_ms = None;

        if uses_ptp {
            let exchange = pending_joins[index]
                .target
                .sender
                .observe_ptp_probe_exchange();
            if let Some(exchange) = exchange {
                projection_ms = Some(clock_ready_delay_ms(
                    exchange,
                    pending_joins[index].target.apple_model,
                ));
                readiness = "projected";
            } else if pending_joins[index]
                .target
                .sender
                .ptp_probe_stalled(Duration::from_millis(AIRPLAY_CLOCK_STALL_MS))
            {
                let pending = pending_joins.remove(index);
                let message = format!(
                    "{}: late join refused because its receiver did not answer the shared PTP clock within {} ms",
                    pending.target.name, AIRPLAY_CLOCK_STALL_MS
                );
                let _ = pending.reply.send(Err(message.clone()));
                if let Ok(mut events) = startup_events.lock() {
                    events.push(message);
                }
                continue;
            } else if pending_joins[index].ready_wait_started.elapsed()
                < Duration::from_millis(AIRPLAY_CLOCK_READY_TIMEOUT_MS)
            {
                index += 1;
                continue;
            } else {
                // Same as MSA ClockReadiness.UNREPORTED: a planning timeout is
                // not a stall verdict, so anchor on the join floor alone.
                readiness = "unreported";
            }
        }

        let now_ntp = match system_time_to_ntp(SystemTime::now()) {
            Ok(value) => value,
            Err(error) => {
                let pending = pending_joins.remove(index);
                let message = format!(
                    "{}: late-join clock conversion failed: {error:?}",
                    pending.target.name
                );
                let _ = pending.reply.send(Err(message.clone()));
                if let Ok(mut events) = startup_events.lock() {
                    events.push(message);
                }
                continue;
            }
        };

        let readiness_lead_ms = projection_ms
            .map(|delay| delay.saturating_add(AIRPLAY_CLOCK_READY_LEAD_MS))
            .unwrap_or(0);
        let floor_ms = AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS.max(readiness_lead_ms);
        let floor_ntp = now_ntp.saturating_add(ms_to_ntp(floor_ms));
        let required_frames = ntp_delta_to_frames_ceil(
            floor_ntp.saturating_sub(effective_start),
            sample_rate,
        );
        let required_packet = required_frames.div_ceil(352);
        // If the requested sample fell out of the retained ring, move the free
        // (not-yet-committed) anchor forward to the oldest sample we still own.
        // Do NOT clamp a future packet back to the current write head: MSA skips
        // live input until that future content position arrives.
        let requested_packet = required_packet.max(oldest_ring_packet);
        let requested_start_ntp = effective_start.saturating_add(frames_to_ntp(
            requested_packet.saturating_mul(352),
            sample_rate,
        ));

        let arm_result = {
            let pending = &mut pending_joins[index];
            pending.target.sender.arm_cold_start_verified(
                requested_start_ntp,
                pending.target.latency_max,
                pending.target.lead_frames,
                pending.target.rtp_offset,
                pending.target.apple_model,
            )
        };
        let committed_start_ntp = match arm_result {
            Ok(value) => value,
            Err(error) => {
                let pending = pending_joins.remove(index);
                let message = format!(
                    "{} late-join START failed: {error:?}",
                    pending.target.name
                );
                let _ = pending.reply.send(Err(message.clone()));
                if let Ok(mut events) = startup_events.lock() {
                    events.push(message);
                }
                continue;
            }
        };

        // The committed instant is the source of truth. Re-map content from it,
        // not from the requested instant, exactly like MSA does after its binary
        // START ack. In today's in-process native sender the two are identical;
        // keeping this remap explicit prevents a future commit correction from
        // silently offsetting a joiner.
        let committed_frames = ntp_delta_to_frames_ceil(
            committed_start_ntp.saturating_sub(effective_start),
            sample_rate,
        );
        let committed_packet = committed_frames
            .div_ceil(352)
            .max(oldest_ring_packet);
        let queued_packets = late_join_ring
            .iter()
            .filter(|(packet, _)| *packet >= committed_packet)
            .map(|(_, pcm)| pcm.clone())
            .collect::<VecDeque<_>>();
        let skip_packets = committed_packet.saturating_sub(packet_index);

        {
            let pending = &mut pending_joins[index];
            pending.anchor_packet = committed_packet;
            pending.skip_packets = skip_packets;
            pending.queued_packets = queued_packets;
            pending.armed = true;

            let rtp_timestamp = pending.target.sender.state().timestamp;
            match pending
                .target
                .metadata
                .send("SAirplay2", "", "", rtp_timestamp)
            {
                Ok(result) if (200..300).contains(&result.status) => {
                    if let Ok(mut events) = startup_events.lock() {
                        events.push(format!(
                            "{}: late-join DMAP metadata {} bytes · RTSP {}.",
                            pending.target.name, result.bytes, result.status
                        ));
                    }
                }
                Ok(result) => {
                    if let Ok(mut events) = startup_events.lock() {
                        events.push(format!(
                            "{}: late-join DMAP metadata rejected · RTSP {}.",
                            pending.target.name, result.status
                        ));
                    }
                }
                Err(error) => {
                    if let Ok(mut events) = startup_events.lock() {
                        events.push(format!(
                            "{}: late-join DMAP metadata failed: {error:?}.",
                            pending.target.name
                        ));
                    }
                }
            }

            let requested_ms = ntp_delta_to_ms(requested_start_ntp);
            let committed_ms = ntp_delta_to_ms(committed_start_ntp);
            let commit_delta = committed_ms as i128 - requested_ms as i128;
            if let Ok(mut events) = startup_events.lock() {
                events.push(format!(
                    "{}: late join clock={} · committed packet #{} · prime={} packet(s) · skip={} packet(s) · START commit delta={:+} ms.",
                    pending.target.name,
                    readiness,
                    committed_packet,
                    pending.queued_packets.len(),
                    skip_packets,
                    commit_delta
                ));
            }
        }

        index += 1;
    }
}

fn ntp_delta_to_ms(value: u64) -> u64 {
    (((value as u128) * 1000) >> 32) as u64
}

fn clock_ready_delay_ms(exchange: crate::PtpExchange, apple_model: bool) -> u64 {
    const CLOCK_LOCK_MS: u64 = 2_300;
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

fn wait_members_clock_projections_ms(
    targets: &[WindowsAudioTarget],
    timeout: Duration,
) -> Vec<(String, Option<u64>)> {
    let ptp_members = targets
        .iter()
        .filter(|target| target.sender.uses_ptp_timing())
        .count();
    if ptp_members == 0 {
        return Vec::new();
    }

    let deadline = Instant::now() + timeout;
    loop {
        let projections = targets
            .iter()
            .filter(|target| target.sender.uses_ptp_timing())
            .map(|target| {
                let projection = target
                    .sender
                    .ptp_probe_exchange()
                    .map(|exchange| clock_ready_delay_ms(exchange, target.apple_model));
                (target.name.clone(), projection)
            })
            .collect::<Vec<_>>();

        // MSA waits for every member's clock result concurrently. Preserve
        // that policy, but retain the per-member result for diagnostics so a
        // Stereo Pair log identifies which receiver was still cold.
        let projected = projections
            .iter()
            .filter(|(_, projection)| projection.is_some())
            .count();
        if projected == ptp_members || Instant::now() >= deadline {
            return projections;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn trim_late_join_ring(
    ring: &mut VecDeque<(u64, Vec<u8>)>,
    source_format: Ap2AudioFormat,
    group_start_ntp: Option<u64>,
    packet_index: u64,
    reanchor_shift_frames: u64,
) {
    let packet_bytes = 352usize.saturating_mul(source_format.input_bytes_per_frame());
    if packet_bytes == 0 {
        ring.clear();
        return;
    }
    let byte_rate = source_format
        .sample_rate
        .saturating_mul(source_format.input_bytes_per_frame() as u32) as f64;

    let mut required_seconds = AIRPLAY_LATE_JOIN_RING_MIN_SECONDS;
    if let (Some(base_start), Ok(now_ntp)) =
        (group_start_ntp, system_time_to_ntp(SystemTime::now()))
    {
        let effective_start =
            base_start.saturating_add(frames_to_ntp(reanchor_shift_frames, source_format.sample_rate));
        let elapsed_frames = ntp_delta_to_frames_ceil(
            now_ntp.saturating_sub(effective_start),
            source_format.sample_rate,
        );
        let write_frames = packet_index.saturating_mul(352);
        let lead_frames = write_frames.saturating_sub(elapsed_frames);
        let lead_seconds = lead_frames as f64 / source_format.sample_rate as f64;
        required_seconds = required_seconds.max(
            lead_seconds + AIRPLAY_LATE_JOIN_RING_MARGIN_SECONDS,
        );
    }

    let wanted_bytes = (required_seconds * byte_rate)
        .ceil()
        .min(AIRPLAY_LATE_JOIN_RING_MAX_BYTES as f64) as usize;
    let max_packets = wanted_bytes.div_ceil(packet_bytes).max(1);
    while ring.len() > max_packets {
        ring.pop_front();
    }
}

fn align_splice_pad(targets: &mut [WindowsAudioTarget]) {
    let max_pad = targets
        .iter()
        .map(|target| target.sender.splice_pad_frames())
        .max()
        .unwrap_or(0);
    for target in targets {
        let current = target.sender.splice_pad_frames();
        if current < max_pad {
            target.sender.add_splice_pad(max_pad - current);
        }
    }
}

fn ms_to_ntp(ms: u64) -> u64 {
    ((ms as u128) << 32).div_ceil(1000) as u64
}

fn resolve_group_start_ntp(requested_start_ntp: u64, floor_ntp: u64) -> u64 {
    if requested_start_ntp >= floor_ntp {
        requested_start_ntp
    } else if requested_start_ntp == 0 {
        floor_ntp
    } else {
        floor_ntp.saturating_add(ms_to_ntp(250))
    }
}

fn frames_to_ntp(frames: u64, sample_rate: u32) -> u64 {
    ((frames as u128) << 32).div_ceil(sample_rate as u128) as u64
}

fn ntp_delta_to_frames_ceil(delta: u64, sample_rate: u32) -> u64 {
    ((delta as u128) * sample_rate as u128).div_ceil(1u128 << 32) as u64
}

#[cfg(test)]
mod mixed_format_tests {
    use super::*;

    #[test]
    fn clock_readiness_matches_msa_full_and_apple_fast_seat() {
        let first = crate::PtpExchange {
            count: 1,
            first_ms: 800,
            last_ms: 0,
            third_ms: 0,
        };
        assert_eq!(clock_ready_delay_ms(first, false), 1_500);
        assert_eq!(clock_ready_delay_ms(first, true), 1_500);

        let seated = crate::PtpExchange {
            count: 3,
            first_ms: 1_000,
            last_ms: 0,
            third_ms: 100,
        };
        assert_eq!(clock_ready_delay_ms(seated, false), 1_300);
        assert_eq!(clock_ready_delay_ms(seated, true), 150);
    }

    #[test]
    fn msa_group_start_convergence_constants_match_source() {
        assert_eq!(AIRPLAY_SPLICE_LEAD_MARGIN_MS, 150);
    }

    #[test]
    fn msa_clock_readiness_floor_can_extend_group_lead() {
        let now = 10u64 << 32;
        let normal = now.saturating_add(ms_to_ntp(AIRPLAY_COLD_GROUP_START_LEAD_MS));
        let projected = now
            .saturating_add(ms_to_ntp(2_300))
            .saturating_add(ms_to_ntp(AIRPLAY_CLOCK_READY_LEAD_MS));
        assert!(projected > normal);
    }

    #[test]
    fn late_join_ring_uses_msa_floor_and_hard_cap() {
        let format = Ap2AudioFormat::ALAC_44100_16_STEREO;
        let packet_bytes = 352 * format.input_bytes_per_frame();
        let floor_packets =
            ((AIRPLAY_LATE_JOIN_RING_MIN_SECONDS * 44_100.0 * 4.0) as usize)
                .div_ceil(packet_bytes);
        assert!(floor_packets > 0);
        assert!(floor_packets * packet_bytes <= AIRPLAY_LATE_JOIN_RING_MAX_BYTES);
    }

    #[test]
    fn late_join_timeline_math_uses_session_sample_rate() {
        let one_second = 1u64 << 32;
        assert_eq!(frames_to_ntp(44_100, 44_100), one_second);
        assert_eq!(frames_to_ntp(48_000, 48_000), one_second);
        assert_eq!(ntp_delta_to_frames_ceil(one_second, 44_100), 44_100);
        assert_eq!(ntp_delta_to_frames_ceil(one_second, 48_000), 48_000);
    }

    #[test]
    fn late_join_48k_packet_duration_is_not_44100_duration() {
        let packets = 10u64;
        let frames = packets * 352;
        let at_48k = frames_to_ntp(frames, 48_000);
        let at_441 = frames_to_ntp(frames, 44_100);
        assert!(at_48k < at_441);
    }

    #[test]
    fn mixed_depth_group_downconverts_s32le_to_s16le() {
        let input = vec![
            0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88,
        ];
        let out = adapt_group_pcm_packet(
            &input,
            Ap2AudioFormat::ALAC_44100_24_STEREO,
            Ap2AudioFormat::ALAC_44100_16_STEREO,
        )
        .unwrap();
        assert_eq!(out, vec![0x33, 0x44, 0x77, 0x88]);
    }

    #[test]
    fn mixed_depth_group_preserves_same_format() {
        let input = vec![1, 2, 3, 4];
        let out = adapt_group_pcm_packet(
            &input,
            Ap2AudioFormat::ALAC_44100_16_STEREO,
            Ap2AudioFormat::ALAC_44100_16_STEREO,
        )
        .unwrap();
        assert_eq!(out, input);
    }
}
