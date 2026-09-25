use crate::{
    system_time_to_ntp, Ap2AudioFormat, NativeMetadataControl, Pcm352Chunker, RealtimeMediaSender,
    WasapiLoopbackCapture, WasapiLoopbackError,
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

pub struct WindowsAudioTarget {
    pub(crate) name: String,
    pub(crate) sender: RealtimeMediaSender,
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
    reply: Sender<Result<(), String>>,
}

#[derive(Clone)]
pub struct WindowsMultiroomJoinHandle {
    command_tx: Sender<GroupAudioCommand>,
}

impl WindowsMultiroomJoinHandle {
    pub fn add_target(&self, target: WindowsAudioTarget) -> Result<(), WindowsMultiroomAudioError> {
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
        reply_rx
            .recv_timeout(Duration::from_secs(12))
            .map_err(|error| {
                WindowsMultiroomAudioError::Command(format!(
                    "late join timed out waiting for the shared timeline: {error}"
                ))
            })?
            .map_err(WindowsMultiroomAudioError::Command)
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
                let mut source_present = false;
                let mut cold_armed = false;
                let mut group_start_ntp: Option<u64> = None;
                let mut input_starved_since: Option<Instant> = None;
                // Stereo Pair owns a fixed two-member hot splice line. Keep its
                // track-gap state separate from MultiRoom membership/recovery.
                let mut pair_nonzero_gap_started: Option<Instant> = None;
                let mut pair_nonzero_gap_reported = false;
                let mut pair_inferred_idle = false;
                let mut pair_idle_keepalive_reported = false;
                let mut pair_transition_epoch = 0u64;
                // MultiRoom has dynamic membership, so its idle/boundary state
                // is deliberately independent from Stereo Pair.
                let mut multi_nonzero_gap_started: Option<Instant> = None;
                let mut multi_nonzero_gap_reported = false;
                let mut multi_inferred_idle = false;
                let mut multi_idle_keepalive_reported = false;
                let mut multi_transition_epoch = 0u64;
                let mut packet_index = 0u64;
                let mut pending_joins = Vec::<PendingJoin>::new();
                let mut late_join_ring = VecDeque::<(u64, Vec<u8>)>::new();

                while running_thread.load(Ordering::SeqCst) {
                    handle_group_commands(
                        &command_rx,
                        &mut targets,
                        &mut pending_joins,
                        cold_armed,
                        group_start_ntp,
                        packet_index,
                        &active_members_thread,
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

                    if kind == WindowsGroupAudioKind::StereoPair && cold_armed {
                        if report.first_nonzero_frame_offset.is_some() {
                            if let Some(started) = pair_nonzero_gap_started.take() {
                                let gap_ms = started.elapsed().as_millis();
                                if gap_ms >= 100 {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "Stereo Pair transition: nonzero PCM resumed after {} ms · wasapi_frames={} · discontinuities={} · pending_bytes={}.",
                                            gap_ms,
                                            frames,
                                            report.discontinuities,
                                            chunker.pending_bytes()
                                        ));
                                    }
                                }
                            }
                            if pair_inferred_idle {
                                if let Ok(now_ntp) = system_time_to_ntp(SystemTime::now()) {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        let heads = targets
                                            .iter()
                                            .map(|target| {
                                                let delta = target.sender.timeline_head_delta_frames(now_ntp);
                                                format!("{}={}f", target.name, delta)
                                            })
                                            .collect::<Vec<_>>()
                                            .join(", ");
                                        events.push(format!(
                                            "Stereo Pair transition: boundary #{} resume · heads=[{}] · pending_bytes={} · anchor/seq/timestamp preserved.",
                                            pair_transition_epoch,
                                            heads,
                                            chunker.pending_bytes()
                                        ));
                                    }
                                }
                                pair_inferred_idle = false;
                                pair_idle_keepalive_reported = false;
                                input_starved_since = None;
                            }
                            pair_nonzero_gap_reported = false;
                        } else if chunker.pending_nonzero_bytes() != 0 {
                            // A 2.5 s Stereo Pair cold/shared start can leave a large
                            // valid PCM backlog in the local chunker. A quiet current
                            // WASAPI drain is not a track boundary while that backlog
                            // still contains real audio; resetting the timer here
                            // prevents local cleanup from discarding queued content.
                            pair_nonzero_gap_started = None;
                            pair_nonzero_gap_reported = false;
                        } else {
                            let started = pair_nonzero_gap_started.get_or_insert_with(Instant::now);
                            if !pair_nonzero_gap_reported
                                && started.elapsed() >= Duration::from_millis(250)
                            {
                                pair_transition_epoch = pair_transition_epoch.saturating_add(1);
                                let pending_before = chunker.pending_bytes();
                                let pending_nonzero_before = chunker.pending_nonzero_bytes();
                                let dropped_pad = targets
                                    .iter()
                                    .map(|target| target.sender.splice_pad_frames())
                                    .max()
                                    .unwrap_or(0);

                                // Same local warm-boundary semantics as the proven
                                // Single worker: discard only sender-local PCM/pad.
                                // Never RTSP FLUSH and never reset RTP/PTP/crypto.
                                chunker.clear();
                                for target in &mut targets {
                                    target.sender.begin_warm_splice_boundary();
                                }
                                pair_inferred_idle = true;
                                pair_idle_keepalive_reported = false;
                                input_starved_since = None;
                                pair_nonzero_gap_reported = true;

                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "Stereo Pair transition: boundary #{} local cleanup · discarded_bytes={} · stale_nonzero_bytes={} · dropped_pad_frames={} · members={} · seq/timestamp/anchor preserved.",
                                        pair_transition_epoch,
                                        pending_before,
                                        pending_nonzero_before,
                                        dropped_pad,
                                        targets.len()
                                    ));
                                }
                            }
                        }
                    }

                    if kind == WindowsGroupAudioKind::MultiRoom && cold_armed {
                        if report.first_nonzero_frame_offset.is_some() {
                            if let Some(started) = multi_nonzero_gap_started.take() {
                                let gap_ms = started.elapsed().as_millis();
                                if gap_ms >= 100 {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "MultiRoom transition: nonzero PCM resumed after {} ms · wasapi_frames={} · discontinuities={} · pending_bytes={}.",
                                            gap_ms,
                                            frames,
                                            report.discontinuities,
                                            chunker.pending_bytes()
                                        ));
                                    }
                                }
                            }
                            if multi_inferred_idle {
                                if let Ok(now_ntp) = system_time_to_ntp(SystemTime::now()) {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        let heads = targets
                                            .iter()
                                            .map(|target| {
                                                let delta = target.sender.timeline_head_delta_frames(now_ntp);
                                                format!("{}={}f", target.name, delta)
                                            })
                                            .collect::<Vec<_>>()
                                            .join(", ");
                                        events.push(format!(
                                            "MultiRoom transition: boundary #{} resume · heads=[{}] · active_members={} · pending_bytes={} · anchor/seq/timestamp preserved.",
                                            multi_transition_epoch,
                                            heads,
                                            targets.len(),
                                            chunker.pending_bytes()
                                        ));
                                    }
                                }
                                multi_inferred_idle = false;
                                multi_idle_keepalive_reported = false;
                                input_starved_since = None;
                            }
                            multi_nonzero_gap_reported = false;
                        } else if chunker.pending_nonzero_bytes() != 0 {
                            // A cold group can hold seconds of valid queued PCM.
                            // Current capture silence is not a boundary until that
                            // queued content has drained to silence too.
                            multi_nonzero_gap_started = None;
                            multi_nonzero_gap_reported = false;
                        } else {
                            let started = multi_nonzero_gap_started.get_or_insert_with(Instant::now);
                            if !multi_nonzero_gap_reported
                                && started.elapsed() >= Duration::from_millis(250)
                            {
                                multi_transition_epoch = multi_transition_epoch.saturating_add(1);
                                let pending_before = chunker.pending_bytes();
                                let pending_nonzero_before = chunker.pending_nonzero_bytes();
                                let dropped_pad = targets
                                    .iter()
                                    .map(|target| target.sender.splice_pad_frames())
                                    .max()
                                    .unwrap_or(0);

                                // Sender-local cleanup only. MSA's real group
                                // replacement explicitly coordinates receiver
                                // FLUSH/START; Windows system-audio capture has no
                                // application-level Next signal, so silence inference
                                // must never mutate receiver session/timing state.
                                chunker.clear();
                                late_join_ring.clear();
                                for target in &mut targets {
                                    target.sender.begin_warm_splice_boundary();
                                }
                                multi_inferred_idle = true;
                                multi_idle_keepalive_reported = false;
                                input_starved_since = None;
                                multi_nonzero_gap_reported = true;

                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "MultiRoom transition: boundary #{} local cleanup · discarded_bytes={} · stale_nonzero_bytes={} · dropped_pad_frames={} · active_members={} · pending_joins={} · seq/timestamp/anchor preserved.",
                                        multi_transition_epoch,
                                        pending_before,
                                        pending_nonzero_before,
                                        dropped_pad,
                                        targets.len(),
                                        pending_joins.len()
                                    ));
                                }
                            }
                        }
                    }

                    // Before START, source silence is not content. This mirrors
                    // the upstream readiness gate: connect members, prove audio
                    // is flowing, then choose one shared audible anchor.
                    if !source_present {
                        if report.first_non_silent_frame_offset.is_some() {
                            source_present = true;
                        } else {
                            chunker.clear();
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                    }

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
                        let clock_projection_ms = wait_members_clock_projection_ms(
                            &targets,
                            Duration::from_millis(AIRPLAY_CLOCK_READY_TIMEOUT_MS),
                        );
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

                        for target in &mut targets {
                            if let Err(error) = target.sender.arm_cold_start(
                                start_ntp,
                                target.latency_max,
                                target.lead_frames,
                                target.rtp_offset,
                            ) {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!(
                                        "{} cold session START failed: {error:?}",
                                        target.name
                                    ));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }

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
                        group_start_ntp = Some(start_ntp);
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(format!(
                                "AirPlay {} session: {} member(s) ready · shared START={} ms · one WASAPI source.",
                                kind.label(),
                                targets.len(),
                                delay_ms
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

                    if (kind == WindowsGroupAudioKind::StereoPair && pair_inferred_idle)
                        || (kind == WindowsGroupAudioKind::MultiRoom && multi_inferred_idle)
                    {
                        input_starved_since = None;
                    } else if chunker.has_packet() {
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

                    // A Stereo Pair is one fixed audible object. During an inferred
                    // track gap keep both member wires hot with the same source-silence
                    // packet cadence, exactly like Single, while preserving each member's
                    // own RTP sequence/timestamp and shared PTP anchor.
                    if kind == WindowsGroupAudioKind::StereoPair
                        && pair_inferred_idle
                        && !chunker.has_packet()
                        && targets.len() == 2
                    {
                        align_splice_pad(&mut targets);
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
                        if targets[gate_index].sender.can_accept_frames(ntp) {
                            let silence =
                                vec![0u8; crate::ALAC_FRAMES_PER_PACKET * bytes_per_frame];
                            let mut failed = None;
                            for target in &mut targets {
                                let target_packet = match adapt_group_pcm_packet(
                                    &silence,
                                    source_format,
                                    target.sender.audio_format(),
                                ) {
                                    Ok(packet) => packet,
                                    Err(error) => {
                                        failed = Some(format!(
                                            "{} stereo-pair silence format failed: {error}",
                                            target.name
                                        ));
                                        break;
                                    }
                                };
                                if let Err(error) = target.sender.send_pcm_352(
                                    &target_packet,
                                    ntp,
                                    target.lead_frames,
                                ) {
                                    failed = Some(format!(
                                        "{} stereo-pair idle keepalive failed: {error:?}",
                                        target.name
                                    ));
                                    break;
                                }
                            }
                            if let Some(message) = failed {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(message);
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                            packet_index = packet_index.saturating_add(1);
                            if !pair_idle_keepalive_reported {
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "Stereo Pair transition: inferred idle keepalive active · boundary={} · packet={} · members=2.",
                                        pair_transition_epoch,
                                        packet_index
                                    ));
                                }
                                pair_idle_keepalive_reported = true;
                            }
                        }
                    }

                    if kind == WindowsGroupAudioKind::MultiRoom
                        && multi_inferred_idle
                        && !chunker.has_packet()
                        && !targets.is_empty()
                    {
                        align_splice_pad(&mut targets);
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
                        if targets[gate_index].sender.can_accept_frames(ntp) {
                            let silence =
                                vec![0u8; crate::ALAC_FRAMES_PER_PACKET * bytes_per_frame];
                            let mut failed = Vec::<(usize, String)>::new();
                            for (index, target) in targets.iter_mut().enumerate() {
                                let target_packet = match adapt_group_pcm_packet(
                                    &silence,
                                    source_format,
                                    target.sender.audio_format(),
                                ) {
                                    Ok(packet) => packet,
                                    Err(error) => {
                                        failed.push((
                                            index,
                                            format!("{} multi-room silence format failed: {error}", target.name),
                                        ));
                                        continue;
                                    }
                                };
                                if let Err(error) = target.sender.send_pcm_352(
                                    &target_packet,
                                    ntp,
                                    target.lead_frames,
                                ) {
                                    failed.push((
                                        index,
                                        format!("{} multi-room idle keepalive failed: {error:?}", target.name),
                                    ));
                                }
                            }
                            packet_index = packet_index.saturating_add(1);

                            if !failed.is_empty() {
                                for (index, message) in failed.into_iter().rev() {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!("AirPlay member removed: {message}"));
                                    }
                                    targets.remove(index);
                                }
                                active_members_thread.store(targets.len() as u64, Ordering::SeqCst);
                            }

                            if !multi_idle_keepalive_reported {
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "MultiRoom transition: inferred idle keepalive active · boundary={} · packet={} · active_members={} · pending_joins={}.",
                                        multi_transition_epoch,
                                        packet_index,
                                        targets.len(),
                                        pending_joins.len()
                                    ));
                                }
                                multi_idle_keepalive_reported = true;
                            }
                        }
                    }

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
                        if !targets[gate_index].sender.can_accept_frames(ntp) {
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
                                    !pending.queued_packets.is_empty()
                                        && pending.target.sender.can_accept_frames(now_ntp)
                                };
                                if !can_send {
                                    break;
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
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!("AirPlay member removed: {message}"));
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
    group_start_ntp: Option<u64>,
    packet_index: u64,
    active_members: &AtomicU64,
    startup_events: &Mutex<Vec<String>>,
    source_format: Ap2AudioFormat,
    late_join_ring: &VecDeque<(u64, Vec<u8>)>,
) {
    while let Ok(command) = command_rx.try_recv() {
        match command {
            GroupAudioCommand::Add { mut target, reply } => {
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

                let Some(base_start) = group_start_ntp else {
                    let _ = reply.send(Err("shared group timeline is unavailable".into()));
                    continue;
                };
                let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = reply.send(Err(format!(
                            "late-join clock conversion failed: {error:?}"
                        )));
                        continue;
                    }
                };

                // Match upstream late-join semantics: map the joiner's first
                // sample onto the group's effective timeline, including any
                // accumulated starvation/re-anchor shift of the reference member.
                let shift_frames = targets
                    .first()
                    .map(|item| item.sender.reanchor_shifted_frames())
                    .unwrap_or(0);
                let sample_rate = source_format.sample_rate;
                let effective_start =
                    base_start.saturating_add(frames_to_ntp(shift_frames, sample_rate));
                let join_floor =
                    now_ntp.saturating_add(ms_to_ntp(AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS));
                let required_frames = ntp_delta_to_frames_ceil(
                    join_floor.saturating_sub(effective_start),
                    sample_rate,
                );
                let required_packet = required_frames.div_ceil(352);
                let oldest_ring_packet = late_join_ring
                    .front()
                    .map(|(index, _)| *index)
                    .unwrap_or(packet_index);
                // MSA moves the anchor later only when the requested content
                // predates the retained ring. Otherwise prime from history and
                // keep the join floor instead of delaying all the way to the
                // current write head.
                let anchor_packet = required_packet.max(oldest_ring_packet).min(packet_index);
                let start_ntp = effective_start.saturating_add(frames_to_ntp(
                    anchor_packet.saturating_mul(352),
                    sample_rate,
                ));
                let queued_packets = late_join_ring
                    .iter()
                    .filter(|(index, _)| *index >= anchor_packet)
                    .map(|(_, packet)| packet.clone())
                    .collect::<VecDeque<_>>();

                match target.sender.arm_cold_start(
                    start_ntp,
                    target.latency_max,
                    target.lead_frames,
                    target.rtp_offset,
                ) {
                    Ok(()) => {
                        let rtp_timestamp = target.sender.state().timestamp;
                        match target.metadata.send("SAirplay2", "", "", rtp_timestamp) {
                            Ok(result) if (200..300).contains(&result.status) => {
                                if let Ok(mut events) = startup_events.lock() {
                                    events.push(format!(
                                        "{}: late-join DMAP metadata {} bytes · RTSP {}.",
                                        target.name, result.bytes, result.status
                                    ));
                                }
                            }
                            Ok(result) => {
                                if let Ok(mut events) = startup_events.lock() {
                                    events.push(format!(
                                        "{}: late-join DMAP metadata rejected · RTSP {}.",
                                        target.name, result.status
                                    ));
                                }
                            }
                            Err(error) => {
                                if let Ok(mut events) = startup_events.lock() {
                                    events.push(format!(
                                        "{}: late-join DMAP metadata failed: {error:?}.",
                                        target.name
                                    ));
                                }
                            }
                        }
                        if let Ok(mut events) = startup_events.lock() {
                            events.push(format!(
                                "{}: late join armed at packet #{} with {} queued prime packet(s) and {} ms minimum headroom.",
                                target.name,
                                anchor_packet,
                                queued_packets.len(),
                                AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS
                            ));
                        }
                        pending_joins.push(PendingJoin {
                            target,
                            queued_packets,
                            anchor_packet,
                            reply,
                        });
                    }
                    Err(error) => {
                        let _ = reply.send(Err(format!(
                            "{} late-join START failed: {error:?}",
                            target.name
                        )));
                    }
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

fn wait_members_clock_projection_ms(
    targets: &[WindowsAudioTarget],
    timeout: Duration,
) -> Option<u64> {
    let ptp_members = targets
        .iter()
        .filter(|target| target.sender.uses_ptp_timing())
        .count();
    if ptp_members == 0 {
        return None;
    }

    let deadline = Instant::now() + timeout;
    loop {
        let projections = targets
            .iter()
            .filter(|target| target.sender.uses_ptp_timing())
            .filter_map(|target| {
                target
                    .sender
                    .ptp_probe_exchange()
                    .map(|exchange| clock_ready_delay_ms(exchange, target.apple_model))
            })
            .collect::<Vec<_>>();

        // MSA waits for every member's clock result concurrently. Here the
        // worker has the same receiver-probe evidence locally, so stop waiting
        // once every PTP member has produced a projection.
        if projections.len() == ptp_members || Instant::now() >= deadline {
            return projections.into_iter().max();
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
