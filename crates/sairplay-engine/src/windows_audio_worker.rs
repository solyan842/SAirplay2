use crate::{
    buffered_anchor_start, system_time_to_ntp, BufferedAnchorStartConfig,
    BufferedMediaSender, BufferedWriteOutcome, Pcm352Chunker, PtpClock,
    RealtimeMediaSender, SharedCseq, SharedRtspControl, WasapiLoopbackCapture,
    WasapiLoopbackError,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub enum WindowsAudioWorkerError {
    Capture(WasapiLoopbackError),
    Media(String),
    Time(String),
}

impl fmt::Display for WindowsAudioWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(e) => write!(f, "{e}"),
            Self::Media(e) => write!(f, "{e}"),
            Self::Time(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WindowsAudioWorkerError {}

pub struct WindowsAudioWorker {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
    first_non_silent_frame: Arc<AtomicU64>,
    startup_events: Arc<Mutex<Vec<String>>>,
}

impl WindowsAudioWorker {
    pub fn start_buffered(
        mut sender: BufferedMediaSender,
        clock: PtpClock,
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        session_uri: String,
        dacp_id: String,
        active_remote: String,
        cold_start_delay_ms: u64,
    ) -> Result<Self, WindowsAudioWorkerError> {
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

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let audio_format = sender.audio_format();
        let bytes_per_frame = audio_format.input_bytes_per_frame();

        let worker = thread::spawn(move || {
            let capture = match WasapiLoopbackCapture::open_default_for_format(audio_format) {
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

            while running_thread.load(Ordering::SeqCst) {
                match capture.drain_into(&mut chunker) {
                    Ok(report) => {
                        if report.discontinuities != 0 {
                            let cumulative = discontinuities_thread
                                .fetch_add(report.discontinuities, Ordering::SeqCst)
                                .saturating_add(report.discontinuities);
                            if let Some(offset) = report.discontinuity_frame_offset {
                                last_discontinuity_frame_thread.store(
                                    captured_frames_total.saturating_add(offset),
                                    Ordering::SeqCst,
                                );
                            }
                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "Buffered: WASAPI discontinuity · count={} · cumulative={}.",
                                    report.discontinuities, cumulative
                                ));
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
                        captured_frames_total =
                            captured_frames_total.saturating_add(report.frames as u64);

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
                                thread::sleep(Duration::from_millis(1));
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
                            let start_ntp =
                                now_ntp.saturating_add(ms_to_ntp(cold_start_delay_ms));
                            sender.arm_cold_start(start_ntp);
                            let anchor_config = BufferedAnchorStartConfig {
                                session_uri: session_uri.clone(),
                                dacp_id: dacp_id.clone(),
                                active_remote: active_remote.clone(),
                                rtp_time: sender.state().timestamp,
                                commanded_start_ntp: start_ntp,
                            };
                            let anchor_result = {
                                let mut guard = match control.lock() {
                                    Ok(guard) => guard,
                                    Err(_) => {
                                        if let Ok(mut slot) = last_error_thread.lock() {
                                            *slot = Some("buffered RTSP control mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                buffered_anchor_start(
                                    &mut guard,
                                    next_cseq.as_ref(),
                                    &clock,
                                    &anchor_config,
                                )
                            };
                            match anchor_result {
                                Ok(anchor_ns) => {
                                    sender.mark_anchored();
                                    cold_armed = true;
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "Buffered startup: anchor armed · delay={} ms · anchor_ns={} · rtp={} · format={}/{}.",
                                            cold_start_delay_ms,
                                            anchor_ns,
                                            sender.state().timestamp,
                                            audio_format.bit_depth,
                                            audio_format.sample_rate
                                        ));
                                    }
                                }
                                Err(error) => {
                                    if let Ok(mut slot) = last_error_thread.lock() {
                                        *slot = Some(format!("buffered cold START failed: {error:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        loop {
                            if !chunker.has_packet() {
                                break;
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
                            match sender.can_accept_frames(now_ntp) {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(error) => {
                                    if let Ok(mut slot) = last_error_thread.lock() {
                                        *slot = Some(format!("buffered pacing failed: {error:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }

                            let Some(packet) = chunker.pop_packet() else { break; };
                            match sender.send_pcm_352(&packet) {
                                Ok(BufferedWriteOutcome::Sent | BufferedWriteOutcome::Backpressured) => {}
                                Err(error) => {
                                    if let Ok(mut slot) = last_error_thread.lock() {
                                        *slot = Some(format!("buffered media send failed: {error:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        if report.frames == 0 {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                    Err(error) => {
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some(error.to_string());
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
                first_non_silent_frame,
                startup_events,
            }),
            Ok(Err(message)) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(WindowsAudioWorkerError::Media(message))
            }
            Err(_) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(WindowsAudioWorkerError::Media(
                    "buffered audio worker did not become ready".into(),
                ))
            }
        }
    }

    /// Starts a dedicated Windows audio thread.
    ///
    /// The worker owns the WASAPI COM apartment, capture client, chunker and
    /// realtime sender on the same thread. This avoids crossing COM apartment
    /// boundaries from the GUI thread.
    pub fn start(
        mut sender: RealtimeMediaSender,
        lead_frames: u32,
        latency_max: Option<u32>,
        rtp_offset: u32,
        cold_start_delay_ms: u64,
    ) -> Result<Self, WindowsAudioWorkerError> {
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

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let audio_format = sender.audio_format();
        let bytes_per_frame = audio_format.input_bytes_per_frame();

        let worker = thread::spawn(move || {
            let capture = match WasapiLoopbackCapture::open_default_for_format(audio_format) {
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
            let mut startup_packet_index: u32 = 0;
            let mut startup_started: Option<std::time::Instant> = None;
            let mut input_starved_since: Option<std::time::Instant> = None;
            let mut nonzero_gap_started: Option<std::time::Instant> = None;
            let mut nonzero_gap_reported = false;
            let mut resume_packet_pending = false;
            let mut transition_packet_diag_remaining: u32 = 0;
            let mut transition_packet_diag_index: u32 = 0;
            let mut transition_epoch: u64 = 0;
            let mut last_ptp_probe_alive: Option<bool> = None;
            let mut last_ptp_snapshot = std::time::Instant::now();
            let mut last_steady_diag = std::time::Instant::now();

            while running_thread.load(Ordering::SeqCst) {
                match capture.drain_into(&mut chunker) {
                    Ok(report) => {
                        if report.discontinuities != 0 {
                            let cumulative = discontinuities_thread
                                .fetch_add(report.discontinuities, Ordering::SeqCst)
                                .saturating_add(report.discontinuities);
                            let absolute_frame = report
                                .discontinuity_frame_offset
                                .map(|offset| captured_frames_total.saturating_add(offset));
                            if let Some(frame) = absolute_frame {
                                last_discontinuity_frame_thread.store(frame, Ordering::SeqCst);
                            }
                            if cold_armed {
                                transition_packet_diag_remaining = 32;
                                transition_packet_diag_index = 0;
                                let state = sender.state();
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    events.push(format!(
                                        "Transition: WASAPI discontinuity · count={} · cumulative={} · frame={:?} · seq={} ts={} · pending_bytes={} · pad_debt={}.",
                                        report.discontinuities,
                                        cumulative,
                                        absolute_frame,
                                        state.sequence,
                                        state.timestamp,
                                        chunker.pending_bytes(),
                                        sender.splice_pad_frames()
                                    ));
                                }
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

                        // SOURCE-ALIGNED DIAGNOSTIC ONLY: upstream monitors the
                        // receiver's uninterrupted Delay_Req/Pdelay_Req probe
                        // streak as clock-readiness evidence. Do the same here,
                        // without changing PCM, RTP, anchors or session state.
                        if cold_armed && sender.uses_ptp_timing() {
                            let exchange = sender.ptp_probe_exchange();
                            let alive = exchange.is_some();
                            let state_changed = last_ptp_probe_alive != Some(alive);
                            let snapshot_due =
                                last_ptp_snapshot.elapsed() >= Duration::from_secs(30);
                            if state_changed || snapshot_due {
                                if let Ok(mut events) = startup_events_thread.lock() {
                                    match exchange {
                                        Some(ex) => events.push(format!(
                                            "Diagnostic: PTP probe streak alive · exchanges={} · streak_age_ms={} · last_probe_age_ms={} · third_probe_age_ms={}.",
                                            ex.count,
                                            ex.first_ms,
                                            ex.last_ms,
                                            ex.third_ms
                                        )),
                                        None => events.push(
                                            "Diagnostic: PTP probe streak unavailable · no Delay_Req/Pdelay_Req seen within 3 s.".into()
                                        ),
                                    }
                                }
                                last_ptp_snapshot = std::time::Instant::now();
                            }
                            last_ptp_probe_alive = Some(alive);
                        }

                        // Windows has no explicit player FLUSH/START command pipe.
                        // Match MSA: digital-zero PCM is still valid PCM, not an
                        // implicit PAUSE/FLUSH/START boundary. Only explicit session
                        // commands or a genuinely dry input path may alter splice
                        // state. Keep zero intervals diagnostic-only and let the PCM
                        // continue through the normal packet/pacing path unchanged.
                        if cold_armed {
                            if report.first_nonzero_frame_offset.is_some() {
                                if let Some(gap_started) = nonzero_gap_started.take() {
                                    let gap_ms = gap_started.elapsed().as_millis();
                                    if gap_ms >= 100 {
                                        if let Ok(mut events) = startup_events_thread.lock() {
                                            events.push(format!(
                                                "Transition: nonzero PCM resumed after {} ms · wasapi_frames={} · discontinuities={} · pending_bytes={} · pad_debt={} · reanchors={}.",
                                                gap_ms,
                                                frames,
                                                report.discontinuities,
                                                chunker.pending_bytes(),
                                                sender.splice_pad_frames(),
                                                sender.timeline_reanchors()
                                            ));
                                        }
                                        resume_packet_pending = true;
                                        transition_packet_diag_remaining = 32;
                                        transition_packet_diag_index = 0;
                                    }
                                }
                                nonzero_gap_reported = false;
                            } else {
                                let gap_started = nonzero_gap_started
                                    .get_or_insert_with(std::time::Instant::now);
                                if !nonzero_gap_reported
                                    && gap_started.elapsed() >= Duration::from_millis(250)
                                {
                                    transition_epoch = transition_epoch.saturating_add(1);
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "Diagnostic: digital-zero PCM >=250 ms · zero_gap={} · wasapi_frames={} · pending_bytes={} · pad_debt={} · reanchors={} · timeline unchanged.",
                                            transition_epoch,
                                            frames,
                                            chunker.pending_bytes(),
                                            sender.splice_pad_frames(),
                                            sender.timeline_reanchors()
                                        ));
                                    }
                                    nonzero_gap_reported = true;
                                }
                            }
                        }

                        // Before the first START, upstream has no live wire feed.
                        // Shared-mode WASAPI may still emit engine SILENT buffers
                        // while no application audio exists; those are equivalent
                        // to "no stdin bytes yet", not content to stream.
                        if !source_present {
                            if report.first_non_silent_frame_offset.is_some() {
                                source_present = true;
                            } else {
                                chunker.clear();
                                thread::sleep(Duration::from_millis(1));
                                continue;
                            }
                        }

                        // Cold START is committed only once a complete 352-frame
                        // transport packet is buffered, matching airplay-cli's
                        // audio-buffered gate. Nothing is sent before this point.
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
                            let start_ntp =
                                now_ntp.saturating_add(ms_to_ntp(cold_start_delay_ms));
                            if let Err(error) = sender.arm_cold_start(
                                start_ntp,
                                latency_max,
                                lead_frames,
                                rtp_offset,
                            ) {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!("cold START failed: {error:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                            cold_armed = true;
                            startup_started = Some(std::time::Instant::now());
                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "Startup: cold START armed · delay={} ms · lead_frames={} · pending_bytes={}.",
                                    cold_start_delay_ms,
                                    lead_frames,
                                    chunker.pending_bytes()
                                ));
                            }
                        }

                        let recovery_ntp = match system_time_to_ntp(SystemTime::now()) {
                            Ok(value) => value,
                            Err(error) => {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };

                        if cold_armed && last_steady_diag.elapsed() >= Duration::from_secs(1) {
                            let state = sender.state();
                            let head_delta = sender.timeline_head_delta_frames(recovery_ntp);
                            let head_delta_ms =
                                head_delta as f64 * 1000.0 / audio_format.sample_rate as f64;
                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "Diagnostic: steady timeline · frames={} · seq={} ts={} · head_delta_frames={} ({:.1} ms) · pending_bytes={} · nonzero_bytes={} · pad_debt={} · gap_ms={}.",
                                    frames,
                                    state.sequence,
                                    state.timestamp,
                                    head_delta,
                                    head_delta_ms,
                                    chunker.pending_bytes(),
                                    chunker.pending_nonzero_bytes(),
                                    sender.splice_pad_frames(),
                                    nonzero_gap_started
                                        .map(|started| started.elapsed().as_millis())
                                        .unwrap_or(0)
                                ));
                            }
                            last_steady_diag = std::time::Instant::now();
                        }

                        // Match MSA recovery semantics. Digital-zero PCM remains
                        // ordinary queued content. Delivery-gap recovery applies
                        // when a complete packet is queued; starvation recovery is
                        // reserved for a genuinely dry input path (frames == 0).
                        if chunker.has_packet() {
                            input_starved_since = None;
                            if let Some(added) = sender.recover_delivery_gap(recovery_ntp, lead_frames) {
                                let startup_window = startup_started
                                    .map(|t| t.elapsed() <= Duration::from_secs(3))
                                    .unwrap_or(false);
                                if startup_window || nonzero_gap_started.is_some() || resume_packet_pending {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "{}: delivery-gap recovery added {} silence frames · total_pad={} · pending_bytes={}.",
                                            if startup_window { "Startup" } else { "Transition" },
                                            added,
                                            sender.splice_pad_frames(),
                                            chunker.pending_bytes()
                                        ));
                                    }
                                }
                            }
                        } else if frames > 0 {
                            // New PCM arrived but not enough for a complete packet;
                            // this is normal producer cadence, not starvation.
                            input_starved_since = None;
                        } else {
                            // Upstream blocks ap2_session_read(..., 250) and only
                            // enters starvation recovery after that full timeout.
                            // WASAPI polling returns ordinary empty drains every
                            // ~1 ms, so never treat a single empty poll as a gap.
                            let started = input_starved_since.get_or_insert_with(std::time::Instant::now);
                            if started.elapsed() >= Duration::from_millis(250) {
                                if let Some(added) = sender.recover_input_gap(recovery_ntp, lead_frames) {
                                    let startup_window = startup_started
                                        .map(|t| t.elapsed() <= Duration::from_secs(3))
                                        .unwrap_or(false);
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "{}: input-gap recovery after >=250 ms added {} silence frames · total_pad={} · pending_bytes={}.",
                                            if startup_window { "Startup" } else { "Transition" },
                                            added,
                                            sender.splice_pad_frames(),
                                            chunker.pending_bytes()
                                        ));
                                    }
                                }
                                input_starved_since = Some(std::time::Instant::now());
                            }
                        }

                        // No synthetic idle mode from sample values. Explicit MSA
                        // PAUSE/EOF state is not inferred from Windows PCM content;
                        // zero PCM reaches the normal send loop as real silence.

                        loop {
                            let pad_now = sender.splice_pad_frames().min(352);
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

                            // Match upstream ap2cl_accept_frames(): queued PCM
                            // remains untouched until the pacing window opens.
                            if !sender.can_accept_frames(ntp) {
                                thread::sleep(Duration::from_millis(1));
                                break;
                            }

                            let head_delta_before = sender.timeline_head_delta_frames(ntp);
                            let head_delta_before_ms =
                                head_delta_before as f64 * 1000.0 / audio_format.sample_rate as f64;
                            let packet = chunker
                                .pop_packet_with_silence_prefix(pad_now)
                                .expect("required real-byte count checked");
                            match sender.send_pcm_352(&packet, ntp, lead_frames) {
                                Ok(result) => {
                                    startup_packet_index = startup_packet_index.saturating_add(1);
                                    let expected_sync =
                                        result.first_marker || result.sequence_sent % 100 == 0;
                                    if !result.audio_delivered
                                        || (expected_sync && !result.sync_sent)
                                    {
                                        if let Ok(mut events) = startup_events_thread.lock() {
                                            events.push(format!(
                                                "Diagnostic: media delivery anomaly · seq={} ts={} marker={} sync_expected={} sync_sent={} audio_sent={} pad_before={}.",
                                                result.sequence_sent,
                                                result.timestamp_sent,
                                                result.first_marker,
                                                expected_sync,
                                                result.sync_sent,
                                                result.audio_delivered,
                                                pad_now
                                            ));
                                        }
                                    }
                                    if resume_packet_pending {
                                        if let Ok(mut events) = startup_events_thread.lock() {
                                            events.push(format!(
                                                "Transition: first outbound after PCM resume · zero_gap={} · seq={} ts={} marker={} sync_sent={} audio_sent={} pad_before={} · pending_after={}.",
                                                transition_epoch,
                                                result.sequence_sent,
                                                result.timestamp_sent,
                                                result.first_marker,
                                                result.sync_sent,
                                                result.audio_delivered,
                                                pad_now,
                                                chunker.pending_bytes()
                                            ));
                                        }
                                        resume_packet_pending = false;
                                    }
                                    if transition_packet_diag_remaining > 0 {
                                        transition_packet_diag_index =
                                            transition_packet_diag_index.saturating_add(1);
                                        if let Ok(mut events) = startup_events_thread.lock() {
                                            events.push(format!(
                                                "Transition: packet diag · format={}-bit/{}Hz · zero_gap={} · packet={}/32 · seq={} ts={} · alac={} B · wire={} B · head_delta_before={} ({:.2} ms) · wasapi_discontinuities={} · pad_before={} · pending_after={}.",
                                                audio_format.bit_depth,
                                                audio_format.sample_rate,
                                                transition_epoch,
                                                transition_packet_diag_index,
                                                result.sequence_sent,
                                                result.timestamp_sent,
                                                result.alac_payload_len,
                                                result.wire_packet_len,
                                                head_delta_before,
                                                head_delta_before_ms,
                                                discontinuities_thread.load(Ordering::SeqCst),
                                                pad_now,
                                                chunker.pending_bytes()
                                            ));
                                        }
                                        transition_packet_diag_remaining -= 1;
                                    }
                                    if startup_started.map(|t| t.elapsed() <= Duration::from_secs(3)).unwrap_or(false)
                                        && (startup_packet_index <= 10 || result.sync_sent || !result.audio_delivered)
                                    {
                                        if let Ok(mut events) = startup_events_thread.lock() {
                                            events.push(format!(
                                                "Startup: RTP #{} seq={} ts={} marker={} sync_sent={} audio_sent={} pad_before={}.",
                                                startup_packet_index,
                                                result.sequence_sent,
                                                result.timestamp_sent,
                                                result.first_marker,
                                                result.sync_sent,
                                                result.audio_delivered,
                                                pad_now
                                            ));
                                        }
                                    }
                                }
                                Err(error) => {
                                    if let Ok(mut slot) = last_error_thread.lock() {
                                        *slot = Some(format!("media send failed: {error:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                            sender.consume_splice_pad(pad_now);
                        }

                        if frames == 0 {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                    Err(error) => {
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some(error.to_string());
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
                first_non_silent_frame,
                startup_events,
            }),
            Ok(Err(message)) => {
                let _ = worker.join();
                Err(WindowsAudioWorkerError::Capture(
                    WasapiLoopbackError::Windows(message),
                ))
            }
            Err(error) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(WindowsAudioWorkerError::Capture(
                    WasapiLoopbackError::Windows(format!(
                        "WASAPI worker startup timeout: {error}"
                    )),
                ))
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
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
        match self.startup_events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(_) => Vec::new(),
        }
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn ms_to_ntp(ms: u64) -> u64 {
    ((ms as u128) << 32).div_ceil(1000) as u64
}

fn ptp_probe_summary(sender: &RealtimeMediaSender) -> String {
    if !sender.uses_ptp_timing() {
        return "ptp_probe=not-applicable".into();
    }
    match sender.ptp_probe_exchange() {
        Some(ex) => format!(
            "ptp_probe=alive exchanges={} streak_age_ms={} last_probe_age_ms={} third_probe_age_ms={}",
            ex.count, ex.first_ms, ex.last_ms, ex.third_ms
        ),
        None => "ptp_probe=unavailable".into(),
    }
}

impl Drop for WindowsAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
