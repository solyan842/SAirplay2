use crate::{
    buffered_anchor_start, system_time_to_ntp, BufferedAnchorStartConfig,
    BufferedMediaSender, BufferedWriteOutcome, PtpClock, RealtimeMediaSender,
    SharedCseq, SharedRtspControl, WasapiLoopbackError, WindowsPcmSession,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

const AIRPLAY_CLOCK_READY_TIMEOUT_MS: u64 = 2_500;
const AIRPLAY_CLOCK_READY_LEAD_MS: u64 = 500;

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
        apple_model: bool,
    ) -> Result<Self, WindowsAudioWorkerError> {
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let last_error = Arc::new(Mutex::new(None));
        let last_error_thread = Arc::clone(&last_error);
        let discontinuities = Arc::new(AtomicU64::new(0));
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let last_discontinuity_frame_thread = Arc::clone(&last_discontinuity_frame);
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));
        let startup_events_thread = Arc::clone(&startup_events);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let audio_format = sender.audio_format();
        let bytes_per_frame = audio_format.input_bytes_per_frame();

        let worker = thread::spawn(move || {
            let mut pcm_session = match WindowsPcmSession::start(audio_format) {
                Ok(session) => {
                    let _ = ready_tx.send(Ok(()));
                    session
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

            let packet_bytes = pcm_session.packet_bytes();
            let mut cold_armed = false;
            let mut clock_wait_started: Option<std::time::Instant> = None;

            while running_thread.load(Ordering::SeqCst) {
                for event in pcm_session.drain_discontinuity_events() {
                    discontinuities_thread.store(event.cumulative, Ordering::SeqCst);
                    if let Some(frame) = event.absolute_frame {
                        last_discontinuity_frame_thread.store(frame, Ordering::SeqCst);
                    }
                    if let Ok(mut events) = startup_events_thread.lock() {
                        events.push(format!(
                            "Buffered: WASAPI discontinuity · count={} · cumulative={} · frame={:?} · session_buffer={} ms.",
                            event.count,
                            event.cumulative,
                            event.absolute_frame,
                            pcm_session.buffered_ms()
                        ));
                    }
                }

                if !cold_armed {
                    match pcm_session.wait_ready(packet_bytes, Duration::from_millis(10)) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(error);
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }

                    // Match MSA: receiver-clock planning happens while the
                    // persistent input reader continues filling the session ring.
                    let clock_projection_ms = if let Some(exchange) = clock.exchange() {
                        clock_wait_started = None;
                        Some(worker_clock_ready_delay_ms(exchange, apple_model))
                    } else {
                        let waiting_since =
                            clock_wait_started.get_or_insert_with(std::time::Instant::now);
                        if waiting_since.elapsed()
                            < Duration::from_millis(AIRPLAY_CLOCK_READY_TIMEOUT_MS)
                        {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        None
                    };
                    let readiness_lead_ms = clock_projection_ms
                        .map(|delay| delay.saturating_add(AIRPLAY_CLOCK_READY_LEAD_MS))
                        .unwrap_or(0);
                    let effective_start_delay_ms =
                        cold_start_delay_ms.max(readiness_lead_ms);

                    let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let requested_start_ntp =
                        now_ntp.saturating_add(ms_to_ntp(effective_start_delay_ms));
                    let mut floor_ntp = now_ntp.saturating_add(ms_to_ntp(250));
                    if let Some(exchange) = clock.exchange() {
                        floor_ntp = floor_ntp.max(
                            now_ntp.saturating_add(ms_to_ntp(
                                worker_clock_ready_delay_ms(exchange, apple_model),
                            )),
                        );
                    }
                    let committed_start_ntp =
                        resolve_worker_start_ntp(requested_start_ntp, floor_ntp);
                    let correction_ms = ntp_delta_ms(
                        committed_start_ntp.saturating_sub(requested_start_ntp),
                    );

                    sender.arm_cold_start(committed_start_ntp);
                    let anchor_config = BufferedAnchorStartConfig {
                        session_uri: session_uri.clone(),
                        dacp_id: dacp_id.clone(),
                        active_remote: active_remote.clone(),
                        rtp_time: sender.state().timestamp,
                        commanded_start_ntp: committed_start_ntp,
                    };
                    let anchor_result = {
                        let mut guard = match control.lock() {
                            Ok(guard) => guard,
                            Err(_) => {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some("buffered RTSP control mutex poisoned".into());
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                break;
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
                                events.push(match clock_projection_ms {
                                    Some(delay) => format!(
                                        "Buffered startup: receiver clock projection={} ms · requested START lead={} ms.",
                                        delay, effective_start_delay_ms
                                    ),
                                    None => format!(
                                        "Buffered startup: no PTP clock projection within {} ms · fallback START lead={} ms.",
                                        AIRPLAY_CLOCK_READY_TIMEOUT_MS, effective_start_delay_ms
                                    ),
                                });
                                if correction_ms > 0 {
                                    events.push(format!(
                                        "Buffered startup: START corrected forward by {} ms before anchor commit.",
                                        correction_ms
                                    ));
                                }
                                events.push(format!(
                                    "Buffered startup: anchor armed · requested_delay={} ms · committed_correction={} ms · anchor_ns={} · rtp={} · format={}/{} · session_buffer={} ms / {} bytes.",
                                    effective_start_delay_ms,
                                    correction_ms,
                                    anchor_ns,
                                    sender.state().timestamp,
                                    audio_format.bit_depth,
                                    audio_format.sample_rate,
                                    pcm_session.buffered_ms(),
                                    pcm_session.capacity_bytes()
                                ));
                            }
                        }
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("buffered cold START failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }

                loop {
                    let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    match sender.can_accept_frames(now_ntp) {
                        Ok(true) => {}
                        Ok(false) => {
                            thread::sleep(Duration::from_millis(1));
                            break;
                        }
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("buffered pacing failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }

                    let packet = match pcm_session
                        .read_exact_timeout(packet_bytes, Duration::from_millis(0))
                    {
                        Ok(Some(packet)) => packet,
                        Ok(None) => break,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(error);
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    match sender.send_pcm_352(&packet) {
                        Ok(BufferedWriteOutcome::Sent | BufferedWriteOutcome::Backpressured) => {}
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("buffered media send failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }

                if pcm_session.buffered_bytes() < packet_bytes {
                    thread::sleep(Duration::from_millis(1));
                }
            }

            pcm_session.stop();
        });
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
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
    /// The transport worker consumes PCM from a persistent session ring. A
    /// dedicated producer thread owns the WASAPI COM apartment/capture client,
    /// so capture keeps draining while START planning, ALAC or network sends
    /// block here; no COM interface crosses thread boundaries.
    pub fn start(
        mut sender: RealtimeMediaSender,
        lead_frames: u32,
        latency_max: Option<u32>,
        rtp_offset: u32,
        cold_start_delay_ms: u64,
        apple_model: bool,
    ) -> Result<Self, WindowsAudioWorkerError> {
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let last_error = Arc::new(Mutex::new(None));
        let last_error_thread = Arc::clone(&last_error);
        let discontinuities = Arc::new(AtomicU64::new(0));
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let last_discontinuity_frame_thread = Arc::clone(&last_discontinuity_frame);
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));
        let startup_events_thread = Arc::clone(&startup_events);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let audio_format = sender.audio_format();
        let bytes_per_frame = audio_format.input_bytes_per_frame();

        let worker = thread::spawn(move || {
            let mut pcm_session = match WindowsPcmSession::start(audio_format) {
                Ok(session) => {
                    let _ = ready_tx.send(Ok(()));
                    session
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

            let packet_bytes = pcm_session.packet_bytes();
            let mut cold_armed = false;
            let mut clock_wait_started: Option<std::time::Instant> = None;
            let mut startup_packet_index: u32 = 0;
            let mut startup_started: Option<std::time::Instant> = None;
            let mut starving = false;
            let mut starvation_started: Option<std::time::Instant> = None;
            let mut last_ptp_probe_alive: Option<bool> = None;
            let mut last_ptp_snapshot = std::time::Instant::now();
            let mut last_steady_diag = std::time::Instant::now();

            while running_thread.load(Ordering::SeqCst) {
                for event in pcm_session.drain_discontinuity_events() {
                    discontinuities_thread.store(event.cumulative, Ordering::SeqCst);
                    if let Some(frame) = event.absolute_frame {
                        last_discontinuity_frame_thread.store(frame, Ordering::SeqCst);
                    }
                    if cold_armed {
                        let state = sender.state();
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(format!(
                                "Transition: WASAPI discontinuity · count={} · cumulative={} · frame={:?} · seq={} ts={} · session_buffer={} ms · pad_debt={}.",
                                event.count,
                                event.cumulative,
                                event.absolute_frame,
                                state.sequence,
                                state.timestamp,
                                pcm_session.buffered_ms(),
                                sender.splice_pad_frames()
                            ));
                        }
                    }
                }

                // Receiver clock evidence is independent of source capture.
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
                                    "Diagnostic: PTP probe streak unavailable · no current Delay_Req/Pdelay_Req evidence.".into()
                                ),
                            }
                        }
                        last_ptp_snapshot = std::time::Instant::now();
                    }
                    last_ptp_probe_alive = Some(alive);
                }

                if !cold_armed {
                    match pcm_session.wait_ready(packet_bytes, Duration::from_millis(10)) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(error);
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }

                    // MSA waits for clock readiness while its persistent reader
                    // keeps filling the source ring. The separate WASAPI reader
                    // gives Windows exactly the same producer/transport split.
                    let clock_projection_ms = if sender.uses_ptp_timing() {
                        if let Some(delay) = sender.ptp_clock_ready_delay_ms(apple_model) {
                            clock_wait_started = None;
                            Some(delay)
                        } else {
                            let waiting_since =
                                clock_wait_started.get_or_insert_with(std::time::Instant::now);
                            if waiting_since.elapsed()
                                < Duration::from_millis(AIRPLAY_CLOCK_READY_TIMEOUT_MS)
                            {
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            None
                        }
                    } else {
                        None
                    };

                    let readiness_lead_ms = clock_projection_ms
                        .map(|delay| delay.saturating_add(AIRPLAY_CLOCK_READY_LEAD_MS))
                        .unwrap_or(0);
                    let effective_start_delay_ms =
                        cold_start_delay_ms.max(readiness_lead_ms);

                    let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let requested_start_ntp =
                        now_ntp.saturating_add(ms_to_ntp(effective_start_delay_ms));

                    let committed_start_ntp = match sender.arm_cold_start_verified(
                        requested_start_ntp,
                        latency_max,
                        lead_frames,
                        rtp_offset,
                        apple_model,
                    ) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("cold START failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let correction_ms = ntp_delta_ms(
                        committed_start_ntp.saturating_sub(requested_start_ntp),
                    );

                    cold_armed = true;
                    startup_started = Some(std::time::Instant::now());
                    if let Ok(mut events) = startup_events_thread.lock() {
                        events.push(match clock_projection_ms {
                            Some(delay) => format!(
                                "Startup: receiver clock projection={} ms · requested START lead={} ms.",
                                delay, effective_start_delay_ms
                            ),
                            None if sender.uses_ptp_timing() => format!(
                                "Startup: no PTP clock projection within {} ms · fallback START lead={} ms.",
                                AIRPLAY_CLOCK_READY_TIMEOUT_MS, effective_start_delay_ms
                            ),
                            None => format!(
                                "Startup: NTP timing · requested START lead={} ms.",
                                effective_start_delay_ms
                            ),
                        });
                        if correction_ms > 0 {
                            events.push(format!(
                                "Startup: solo START corrected forward by {} ms; committed instant adopted without re-START.",
                                correction_ms
                            ));
                        }
                        events.push(format!(
                            "Startup: cold START armed · requested_delay={} ms · committed_correction={} ms · render_lead_frames={} · session_buffer={} ms / {} bytes.",
                            effective_start_delay_ms,
                            correction_ms,
                            lead_frames,
                            pcm_session.buffered_ms(),
                            pcm_session.capacity_bytes()
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
                        break;
                    }
                };

                if cold_armed && last_steady_diag.elapsed() >= Duration::from_secs(5) {
                    let state = sender.state();
                    let head_delta = sender.timeline_head_delta_frames(recovery_ntp);
                    let head_delta_ms =
                        head_delta as f64 * 1000.0 / audio_format.sample_rate as f64;
                    if let Ok(mut events) = startup_events_thread.lock() {
                        events.push(format!(
                            "Diagnostic: steady timeline · captured_frames={} · seq={} ts={} · head_delta_frames={} ({:.1} ms) · session_buffer={} ms · pad_debt={}.",
                            pcm_session.captured_frames(),
                            state.sequence,
                            state.timestamp,
                            head_delta,
                            head_delta_ms,
                            pcm_session.buffered_ms(),
                            sender.splice_pad_frames()
                        ));
                    }
                    last_steady_diag = std::time::Instant::now();
                }

                // Source-equivalent delivery-stall guard: this runs before the
                // source read so queued content is never emitted on timestamps
                // that have already fallen behind wall time.
                if let Some(added) = sender.recover_delivery_gap(recovery_ntp, lead_frames) {
                    let startup_window = startup_started
                        .map(|t| t.elapsed() <= Duration::from_secs(3))
                        .unwrap_or(false);
                    if startup_window {
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(format!(
                                "Startup: delivery-gap recovery added {} silence frames · total_pad={} · session_buffer={} ms.",
                                added,
                                sender.splice_pad_frames(),
                                pcm_session.buffered_ms()
                            ));
                        }
                    }
                }

                let ntp = match system_time_to_ntp(SystemTime::now()) {
                    Ok(value) => value,
                    Err(error) => {
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        break;
                    }
                };

                if !sender.can_accept_frames(ntp) {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                let pad_now = sender.splice_pad_frames().min(352);
                let pad_bytes = pad_now as usize * bytes_per_frame;
                let real_bytes_needed = packet_bytes.saturating_sub(pad_bytes);

                let real = if real_bytes_needed == 0 {
                    Some(Vec::new())
                } else {
                    match pcm_session.read_exact_timeout(
                        real_bytes_needed,
                        Duration::from_millis(250),
                    ) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(error);
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                };

                let Some(real) = real else {
                    if !starving {
                        starving = true;
                        starvation_started = Some(std::time::Instant::now());
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(
                                "Transition: PCM session input starved; waiting in 250 ms intervals."
                                    .into(),
                            );
                        }
                    }

                    let gap_ntp = match system_time_to_ntp(SystemTime::now()) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    if let Some(added) = sender.recover_input_gap(gap_ntp, lead_frames) {
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(format!(
                                "Transition: input-gap recovery added {} silence frames · total_pad={} · session_buffer={} ms.",
                                added,
                                sender.splice_pad_frames(),
                                pcm_session.buffered_ms()
                            ));
                        }
                    }
                    continue;
                };

                if starving {
                    if let Ok(mut events) = startup_events_thread.lock() {
                        events.push(format!(
                            "Transition: PCM session input recovered after {} ms.",
                            starvation_started
                                .map(|started| started.elapsed().as_millis())
                                .unwrap_or(0)
                        ));
                    }
                    starving = false;
                    starvation_started = None;
                }

                let mut packet = vec![0u8; packet_bytes];
                if !real.is_empty() {
                    packet[pad_bytes..pad_bytes + real.len()].copy_from_slice(&real);
                }

                match sender.send_pcm_352(&packet, ntp, lead_frames) {
                    Ok(result) => {
                        startup_packet_index = startup_packet_index.saturating_add(1);
                        let expected_sync =
                            result.first_marker || result.sequence_sent % 100 == 0;
                        if !result.audio_delivered || (expected_sync && !result.sync_sent) {
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
                        if startup_started
                            .map(|t| t.elapsed() <= Duration::from_secs(3))
                            .unwrap_or(false)
                            && (startup_packet_index <= 10
                                || result.sync_sent
                                || !result.audio_delivered)
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
                        break;
                    }
                }
                sender.consume_splice_pad(pad_now);
            }

            pcm_session.stop();
        });
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
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

fn ntp_delta_ms(delta: u64) -> u64 {
    (((delta as u128) * 1000) >> 32) as u64
}

fn worker_clock_ready_delay_ms(exchange: crate::PtpExchange, apple_model: bool) -> u64 {
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

fn resolve_worker_start_ntp(requested_start_ntp: u64, floor_ntp: u64) -> u64 {
    if requested_start_ntp >= floor_ntp {
        requested_start_ntp
    } else if requested_start_ntp == 0 {
        floor_ntp
    } else {
        floor_ntp.saturating_add(ms_to_ntp(250))
    }
}

impl Drop for WindowsAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
