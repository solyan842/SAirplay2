use crate::{
    system_time_to_ntp, Ap2AudioFormat, NativeMetadataControl, Pcm352Chunker, RealtimeMediaSender,
    WasapiLoopbackCapture, WasapiLoopbackError,
};
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
    start_packet: u64,
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
                let mut packet_index = 0u64;
                let mut pending_joins = Vec::<PendingJoin>::new();

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
                        let settle_ms = targets
                            .iter()
                            .map(|target| target.cold_start_delay_ms)
                            .max()
                            .unwrap_or(0);
                        let delay_ms = source_lead.max(settle_ms);
                        let start_ntp = now_ntp.saturating_add(ms_to_ntp(delay_ms));

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

                    if kind == WindowsGroupAudioKind::StereoPair && pair_inferred_idle {
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

                    loop {
                        if targets.is_empty() && pending_joins.is_empty() {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some("all AirPlay session members stopped".into());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }

                        // A late joiner was armed ahead of time. Attach it
                        // exactly when the live feed reaches the content sample
                        // mapped to that shared group instant. No old source is
                        // replayed and the running members are never paused.
                        let mut index = 0usize;
                        while index < pending_joins.len() {
                            if pending_joins[index].start_packet <= packet_index {
                                let pending = pending_joins.remove(index);
                                let name = pending.target.name.clone();
                                targets.push(pending.target);
                                active_members_thread.store(targets.len() as u64, Ordering::SeqCst);
                                let _ = pending.reply.send(Ok(()));
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "Late joiner {name}: attached at shared content packet #{packet_index}."
                                    ));
                                }
                            } else {
                                index += 1;
                            }
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
                        packet_index = packet_index.saturating_add(1);

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
                let effective_start = base_start.saturating_add(frames_to_ntp(shift_frames));
                let join_floor =
                    now_ntp.saturating_add(ms_to_ntp(AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS));
                let required_frames = ntp_delta_to_frames_ceil(
                    join_floor.saturating_sub(effective_start),
                );
                let required_packet = required_frames.div_ceil(352);
                let start_packet = packet_index.max(required_packet);
                let start_ntp = effective_start.saturating_add(frames_to_ntp(
                    start_packet.saturating_mul(352),
                ));

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
                                "{}: late join armed for packet #{} with {} ms minimum headroom.",
                                target.name,
                                start_packet,
                                AIRPLAY_LATE_JOIN_MIN_HEADROOM_MS
                            ));
                        }
                        pending_joins.push(PendingJoin {
                            target,
                            start_packet,
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

fn frames_to_ntp(frames: u64) -> u64 {
    ((frames as u128) << 32).div_ceil(44_100) as u64
}

fn ntp_delta_to_frames_ceil(delta: u64) -> u64 {
    ((delta as u128) * 44_100u128).div_ceil(1u128 << 32) as u64
}

#[cfg(test)]
mod mixed_format_tests {
    use super::*;

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
