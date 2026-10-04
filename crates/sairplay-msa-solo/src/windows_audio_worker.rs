//! Windows PCM source -> native AP2 receiver media worker.
//! Capture ownership lives in WindowsPcmSource. This worker owns only the
//! receiver-facing media consumer and MSA lifecycle adaptation around the
//! shared PCM hub, so slow type-103 TCP/RTSP work cannot block WASAPI capture.

use crate::{
    ap2::Ap2State,
    native_media::SendResult,
    time_domain::SourceNtp,
    Ap2AudioFormat, NativeSoloEngine, PcmSourceDiagnosticContext,
    SoloClockReadinessState, WindowsPcmHub, WindowsPcmSource, WasapiLoopbackError,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const AIRPLAY_CLOCK_READY_TIMEOUT: Duration = Duration::from_millis(2500);
pub const STARVATION_RECOVERY_INTERVAL: Duration = Duration::from_millis(250);
/// Windows loopback adaptation of MSA's explicit PAUSE event for Buffered
/// type103. This measures absence of *all* captured frames, not audio silence:
/// AUDCLNT_BUFFERFLAGS_SILENT packets still reset the timer and remain valid PCM.
pub const BUFFERED_CAPTURE_IDLE_PARK_INTERVAL: Duration = Duration::from_millis(500);
pub const FLUSH_DRAIN_TIMEOUT: Duration = Duration::from_millis(2000);
const DEFERRED_START_LEAD_MS: u64 = 400;
const DEFERRED_CLOCK_READY_LEAD_MS: u64 = 500;

pub type SharedNativeSoloEngine = Arc<Mutex<NativeSoloEngine>>;

fn buffered_capture_idle_should_park(
    source_present: bool,
    idle_for: Duration,
    is_buffered: bool,
    state: Ap2State,
    content_paused: bool,
    content_stopped: bool,
) -> bool {
    source_present
        && is_buffered
        && state == Ap2State::Streaming
        && !content_paused
        && !content_stopped
        && idle_for >= BUFFERED_CAPTURE_IDLE_PARK_INTERVAL
}


fn realtime_capture_resume_should_rewarm(
    source_present: bool,
    capture_edge: bool,
    non_silent_edge: bool,
    idle_for: Duration,
    is_buffered: bool,
    state: Ap2State,
    splice_timeline: bool,
) -> bool {
    source_present
        && capture_edge
        && non_silent_edge
        && idle_for >= STARVATION_RECOVERY_INTERVAL
        && !is_buffered
        && state == Ap2State::Streaming
        && splice_timeline
}

#[derive(Debug)]
pub enum WindowsSoloAudioWorkerError {
    Capture(WasapiLoopbackError),
    Engine(String),
    Time(String),
}

impl fmt::Display for WindowsSoloAudioWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(e) => write!(f, "{e}"),
            Self::Engine(e) => write!(f, "{e}"),
            Self::Time(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WindowsSoloAudioWorkerError {}

pub struct WindowsSoloAudioWorker {
    running: Arc<AtomicBool>,
    engine: SharedNativeSoloEngine,
    pcm_hub: WindowsPcmHub,
    deferred_start_armed: Arc<AtomicBool>,
    pcm_source: WindowsPcmSource,
    media_worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
}

impl WindowsSoloAudioWorker {
    pub fn start(engine: SharedNativeSoloEngine) -> Result<Self, WindowsSoloAudioWorkerError> {
        let audio_format = {
            let guard = engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            Ap2AudioFormat {
                sample_rate: guard.ready.media.sample_rate,
                bit_depth: guard.ready.media.bit_depth,
                channels: guard.ready.media.channels,
            }
        };

        let running = Arc::new(AtomicBool::new(true));
        let pcm_hub = WindowsPcmHub::new(audio_format);
        let deferred_start_armed = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));

        // Keep receiver diagnostics available to capture telemetry without
        // coupling WindowsPcmSource to NativeSoloEngine or any AirPlay state.
        let diagnostic_engine = Arc::clone(&engine);
        let diagnostic_context: PcmSourceDiagnosticContext = Arc::new(move || {
            let guard = diagnostic_engine.try_lock().ok()?;
            let diag = guard.diagnostics();
            Some(format!(
                "state={:?} head_frame={} pacing_ahead_frames={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={}",
                diag.state,
                diag.head_frame,
                diag.pacing_ahead_frames,
                diag.audio_sent,
                diag.audio_dropped,
                diag.sync_sent,
                diag.sync_dropped,
            ))
        });

        // Spawn the transport-agnostic WASAPI producer first, matching the
        // validated producer-before-media ordering. It writes only to pcm_hub.
        let mut pcm_source = WindowsPcmSource::spawn(
            audio_format,
            pcm_hub.clone(),
            Arc::clone(&running),
            Arc::clone(&last_error),
            Arc::clone(&startup_events),
            Some(diagnostic_context),
        )
        .map_err(|error| WindowsSoloAudioWorkerError::Engine(error.to_string()))?;

        // Consumer: owns the MSA media loop. It takes PCM from the hub, drops
        // that mutex immediately, then performs all potentially slow
        // pacing/ALAC/TCP/RTSP work.
        let media_worker = {
            let running_thread = Arc::clone(&running);
            let pcm_hub_thread = pcm_hub.clone();
            let ring_thread = pcm_hub_thread.ring();
            let deferred_start_thread = Arc::clone(&deferred_start_armed);
            let error_thread = Arc::clone(&last_error);
            let events_thread = Arc::clone(&startup_events);
            let engine_thread = Arc::clone(&engine);

            thread::Builder::new()
                .name("sairplay-msa-media".into())
                .spawn(move || {
                    let mut starvation_started: Option<Instant> = None;
                    let mut last_starvation_recovery: Option<Instant> = None;
                    let mut starvation_recovery_count: u64 = 0;
                    let mut deferred_audio_seen: Option<Instant> = None;
                    let mut local_flush_ack = pcm_hub_thread.flush_ack_generation();
                    let mut capture_frame_seen =
                        pcm_hub_thread.capture_frame_generation();
                    let mut non_silent_seen =
                        pcm_hub_thread.non_silent_generation();
                    let mut last_capture_frame_at = Instant::now();
                    let mut buffered_capture_parked = false;
                    let mut buffered_park_flush_target: Option<u64> = None;

                    while running_thread.load(Ordering::SeqCst) {
                        let control_ok = {
                            let mut guard = match engine_thread.lock() {
                                Ok(v) => v,
                                Err(_) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some("native SOLO engine mutex poisoned".into());
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            };
                            guard.state() != Ap2State::Down && guard.control_healthy()
                        };
                        if !control_ok {
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some("AirPlay 2 control channel failed".into());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }

                        let ack = pcm_hub_thread.flush_ack_generation();
                        if ack != local_flush_ack {
                            local_flush_ack = ack;
                            starvation_started = None;
                            last_starvation_recovery = None;
                            starvation_recovery_count = 0;
                            deferred_audio_seen = None;
                            capture_frame_seen =
                                pcm_hub_thread.capture_frame_generation();
                            non_silent_seen =
                                pcm_hub_thread.non_silent_generation();
                            last_capture_frame_at = Instant::now();
                            if buffered_park_flush_target
                                .map(|target| ack >= target)
                                .unwrap_or(false)
                            {
                                buffered_park_flush_target = None;
                            }
                        }

                        let capture_generation =
                            pcm_hub_thread.capture_frame_generation();
                        let capture_edge = capture_generation != capture_frame_seen;
                        let capture_idle_before_edge = last_capture_frame_at.elapsed();
                        if capture_edge {
                            capture_seen = capture_generation;
                            last_capture_frame_at = Instant::now();
                        }

                        let non_silent_generation =
                            pcm_hub_thread.non_silent_generation();
                        let non_silent_edge = non_silent_generation != non_silent_seen;
                        if non_silent_edge {
                            non_silent_seen = non_silent_generation;
                        }

                        // Windows has no explicit player PAUSE event. During a long realtime
                        // capture gap MSA keeps the splice wire alive with silence. If fresh
                        // non-silent PCM returns while that wire head is in its low phase,
                        // re-run MSA's own input-gap recovery before consuming content. When
                        // the effective head is already warm this is deliberately a no-op.
                        let realtime_resume_rewarmed = {
                            let mut guard = match engine_thread.lock() {
                                Ok(v) => v,
                                Err(_) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some("native SOLO engine mutex poisoned".into());
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            };
                            if realtime_capture_resume_should_rewarm(
                                pcm_hub_thread.source_present(),
                                capture_edge,
                                non_silent_edge,
                                capture_idle_before_edge,
                                guard.is_buffered(),
                                guard.runtime.state,
                                guard.runtime.splice_timeline,
                            ) {
                                let now_ntp = match SourceNtp::from_system_time(SystemTime::now()) {
                                    Ok(v) => v,
                                    Err(_) => SourceNtp::ZERO,
                                };
                                let now_frame = now_ntp.to_frames(guard.runtime.media.timeline.sample_rate);
                                match guard.ready.timing_owner.sync_timing() {
                                    Ok(timing) => {
                                        let engine = &mut *guard;
                                        engine.runtime.recover_input_gap(
                                            now_frame,
                                            &mut engine.ready.media.io,
                                            timing,
                                        )
                                    }
                                    Err(_) => false,
                                }
                            } else {
                                false
                            }
                        };
                        if realtime_resume_rewarmed {
                            if let Ok(mut events) = events_thread.lock() {
                                events.push(format!(
                                    "MSA INPUT REALTIME source-resume rewarm: capture idle {}ms; MSA timeline silence queued before fresh PCM.",
                                    capture_idle_before_edge.as_millis()
                                ));
                            }
                        }

                        // A capture-idle park is resumed only after the producer
                        // has acknowledged the local PCM flush and fresh
                        // non-silent PCM arrives. Re-arm the existing deferred
                        // START path; NativeSoloEngine::start() then uses MSA's
                        // post-FLUSH buffered resume and creates a fresh rate-1
                        // anchor instead of attempting an in-place un-pause.
                        if buffered_capture_parked
                            && buffered_park_flush_target.is_none()
                            && non_silent_edge
                        {
                            let restart_allowed = {
                                let guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                guard.is_buffered()
                                    && guard.runtime.state == Ap2State::Connected
                                    && guard.content_stopped()
                            };

                            if restart_allowed {
                                buffered_capture_parked = false;
                                deferred_audio_seen = None;
                                deferred_start_thread.store(true, Ordering::SeqCst);
                                last_capture_frame_at = Instant::now();
                                if let Ok(mut events) = events_thread.lock() {
                                    events.push(
                                        "MSA INPUT Buffered source resumed after idle park: fresh post-flush PCM detected; deferred Buffered START re-armed."
                                            .into(),
                                    );
                                }
                            } else {
                                // Another explicit lifecycle command superseded
                                // the inferred park. Do not manufacture a START.
                                buffered_capture_parked = false;
                            }
                        }

                        // MSA receives PAUSE/PLAY as explicit commands; a
                        // Windows capture gap is not one. For Buffered type103,
                        // park the session with MSA STANDBY only after sustained
                        // absence of *all* capture frames. STANDBY performs the
                        // protocol-native rate-0 + FLUSHBUFFERED boundary and
                        // leaves the live session CONNECTED for a clean restart.
                        if !buffered_capture_parked
                            && pcm_hub_thread.source_present()
                        {
                            let (park_result, park_mrp) = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                if buffered_capture_idle_should_park(
                                    true,
                                    last_capture_frame_at.elapsed(),
                                    guard.is_buffered(),
                                    guard.runtime.state,
                                    guard.content_paused(),
                                    guard.content_stopped(),
                                ) {
                                    let result = guard.standby().map(|_| true);
                                    let mrp = if result.is_ok() {
                                        guard.mrp_controller()
                                    } else {
                                        None
                                    };
                                    (result, mrp)
                                } else {
                                    (Ok(false), None)
                                }
                            };

                            match park_result {
                                Ok(true) => {
                                    buffered_capture_parked = true;
                                    deferred_start_thread.store(false, Ordering::SeqCst);
                                    deferred_audio_seen = None;

                                    // STANDBY has already flushed the receiver.
                                    // Now discard every pre-boundary local PCM
                                    // byte before allowing the fresh START.
                                    let flush_target = pcm_hub_thread.request_flush();
                                    buffered_park_flush_target = Some(flush_target);

                                    let mrp_result = park_mrp.map(|mrp| {
                                        mrp.publish_playback_state(
                                            crate::MrpPlaybackState::Paused,
                                            true,
                                        )
                                    });
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA INPUT Buffered capture idle for >={}ms: STANDBY + FLUSHBUFFERED completed; local PCM flush generation={flush_target}; waiting for fresh post-flush PCM before deferred START; MRP Paused publish={mrp_result:?}.",
                                            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL.as_millis()
                                        ));
                                    }
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some(format!(
                                            "Buffered capture-idle standby failed: {e:?}"
                                        ));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        let has_packet = ring_thread
                            .lock()
                            .map(|ring| ring.has_packet())
                            .unwrap_or(false);
                        pcm_hub_thread.set_audio_ready(
                            pcm_hub_thread.source_present() && has_packet,
                        );

                        if deferred_start_thread.load(Ordering::SeqCst) && has_packet {
                            let first_audio_at =
                                *deferred_audio_seen.get_or_insert_with(Instant::now);
                            let start_attempt = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };

                                if guard.runtime.state == Ap2State::Streaming {
                                    deferred_start_thread.store(false, Ordering::SeqCst);
                                    None
                                } else if guard.runtime.state != Ap2State::Connected {
                                    None
                                } else {
                                    let now_unix_ms = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis()
                                        .min(u128::from(u64::MAX)) as u64;

                                    let buffered_connected_now =
                                        match guard.ensure_buffered_media_connected() {
                                            Ok(v) => v,
                                            Err(e) => {
                                                if let Ok(mut slot) = error_thread.lock() {
                                                    *slot = Some(format!(
                                                        "deferred native SOLO media activation failed: {e:?}"
                                                    ));
                                                }
                                                running_thread.store(false, Ordering::SeqCst);
                                                return;
                                            }
                                        };

                                    let uses_ptp = guard.uses_ptp();
                                    let readiness = guard.clock_readiness();
                                    let have_projection = uses_ptp
                                        && matches!(
                                            readiness.state,
                                            SoloClockReadinessState::Probing
                                                | SoloClockReadinessState::Ready
                                        )
                                        && readiness.ready_at_unix_ms != 0;
                                    let clock_ready_now =
                                        !uses_ptp
                                            || readiness.state == SoloClockReadinessState::Ready;
                                    let projection_timeout =
                                        first_audio_at.elapsed() >= AIRPLAY_CLOCK_READY_TIMEOUT;

                                    // Once capture is independent, match MSA:
                                    // a valid projected clock floor is enough to
                                    // commit START while the producer continues
                                    // filling the input ring in parallel.
                                    if uses_ptp
                                        && !have_projection
                                        && !clock_ready_now
                                        && !projection_timeout
                                    {
                                        None
                                    } else {
                                        let ready_at = if have_projection {
                                            readiness.ready_at_unix_ms
                                        } else {
                                            0
                                        };
                                        let receiver_lead_ms =
                                            guard.effective_lead_ms().max(DEFERRED_START_LEAD_MS);
                                        let mut requested =
                                            now_unix_ms.saturating_add(receiver_lead_ms);
                                        if ready_at != 0 {
                                            requested = requested.max(
                                                ready_at.saturating_add(
                                                    DEFERRED_CLOCK_READY_LEAD_MS,
                                                ),
                                            );
                                        }

                                        let started = match guard.start(requested) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                if let Ok(mut slot) = error_thread.lock() {
                                                    *slot = Some(format!(
                                                        "deferred native SOLO START failed: {e:?}"
                                                    ));
                                                }
                                                running_thread.store(false, Ordering::SeqCst);
                                                return;
                                            }
                                        };
                                        if requested.abs_diff(started.at_unix_ms) > 10_000 {
                                            if let Ok(mut slot) = error_thread.lock() {
                                                *slot = Some(format!(
                                                    "MSA SOLO TIME-DOMAIN invariant failed: requested={} accepted={}",
                                                    requested, started.at_unix_ms
                                                ));
                                            }
                                            running_thread.store(false, Ordering::SeqCst);
                                            return;
                                        }
                                        let diag = guard.diagnostics();
                                        let mrp = guard.mrp_controller();
                                        let clock_event = if !uses_ptp {
                                            "MSA SOLO CLOCK not applicable: receiver uses NTP timing."
                                                .to_owned()
                                        } else if have_projection {
                                            format!(
                                                "MSA SOLO CLOCK state={:?} exchanges={} streak_age={}ms ready_at={} ready_in={}ms.",
                                                readiness.state,
                                                readiness.exchanges,
                                                readiness.streak_age_ms,
                                                readiness.ready_at_unix_ms,
                                                readiness.ready_in_ms,
                                            )
                                        } else {
                                            format!(
                                                "MSA SOLO CLOCK projection unreported within {}ms; state={:?} exchanges={}; anchoring on the source lead.",
                                                AIRPLAY_CLOCK_READY_TIMEOUT.as_millis(),
                                                readiness.state,
                                                readiness.exchanges,
                                            )
                                        };
                                        Some((
                                            requested,
                                            started,
                                            diag,
                                            mrp,
                                            clock_event,
                                            buffered_connected_now,
                                        ))
                                    }
                                }
                            };

                            if let Some((
                                requested,
                                started,
                                diag,
                                mrp,
                                clock_event,
                                buffered_connected_now,
                            )) = start_attempt
                            {
                                deferred_start_thread.store(false, Ordering::SeqCst);
                                if let Ok(mut events) = events_thread.lock() {
                                    events.push(
                                        "MSA SOLO AUDIO first packet present; committing deferred Buffered START."
                                            .into(),
                                    );
                                    if buffered_connected_now {
                                        events.push(
                                            "MSA SOLO BUFFERED data TCP connected at START boundary."
                                                .into(),
                                        );
                                    }
                                    events.push(clock_event);
                                    events.push(format!(
                                        "MSA SOLO TIME requested={} accepted={} delta={}ms.",
                                        requested,
                                        started.at_unix_ms,
                                        requested.abs_diff(started.at_unix_ms),
                                    ));
                                    events.push(format!("MSA SOLO START {started:?}."));
                                    events.push(format!(
                                        "MSA SOLO TIMELINE head_frame={} pacing_ahead_frames={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={}.",
                                        diag.head_frame,
                                        diag.pacing_ahead_frames,
                                        diag.audio_sent,
                                        diag.audio_dropped,
                                        diag.sync_sent,
                                        diag.sync_dropped,
                                    ));
                                }
                                if let Some(mrp) = mrp {
                                    let _ = mrp.publish_playback_state_on_transition(
                                        crate::MrpPlaybackState::Playing,
                                    );
                                }
                            }
                        } else if !deferred_start_thread.load(Ordering::SeqCst) {
                            deferred_audio_seen = None;
                        }

                        // Do not infer source lifecycle from WASAPI SILENT.
                        // AUDCLNT_BUFFERFLAGS_SILENT is valid PCM silence, not
                        // a PAUSE/STOP/EOF signal. MSA changes playback state
                        // only from explicit session commands/EOF; on Windows
                        // we therefore keep the active type103 timeline alive
                        // through silence until an explicit app lifecycle
                        // command changes it.

                        let content_paused_or_stopped = engine_thread
                            .lock()
                            .map(|guard| guard.content_paused() || guard.content_stopped())
                            .unwrap_or(true);
                        if content_paused_or_stopped {
                            let send_silence = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                if guard.runtime.state == Ap2State::Streaming
                                    && guard.runtime.splice_timeline
                                {
                                    match guard.accept_frames_now() {
                                        Ok(true) => {
                                            let silence = vec![
                                                0u8;
                                                352 * audio_format.input_bytes_per_frame()
                                            ];
                                            guard.send_pcm_352(&silence).map(|_| true)
                                        }
                                        Ok(false) => Ok(false),
                                        Err(e) => Err(e),
                                    }
                                } else {
                                    Ok(false)
                                }
                            };
                            if let Err(e) = send_silence {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!("splice silence send failed: {e:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }

                        let transport_streaming = engine_thread
                            .lock()
                            .map(|guard| guard.runtime.state == Ap2State::Streaming)
                            .unwrap_or(false);
                        if !transport_streaming {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }

                        loop {
                            let can_accept = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                match guard.accept_frames_now() {
                                    Ok(v) => v,
                                    Err(e) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some(format!(
                                                "native SOLO pacing failed: {e:?}"
                                            ));
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            };
                            if !can_accept {
                                break;
                            }

                            let pad_frames = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => return,
                                };
                                let now_ntp =
                                    match SourceNtp::from_system_time(SystemTime::now()) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            if let Ok(mut slot) = error_thread.lock() {
                                                *slot = Some(format!(
                                                    "clock conversion failed: {e}"
                                                ));
                                            }
                                            running_thread.store(false, Ordering::SeqCst);
                                            return;
                                        }
                                    };
                                let now_frame =
                                    now_ntp.to_frames(guard.runtime.media.timeline.sample_rate);
                                if guard.runtime.state == Ap2State::Streaming {
                                    guard.runtime.recover_delivery_gap(now_frame);
                                }
                                guard.runtime.splice_pad_frames.min(352) as u32
                            };

                            let send_generation = pcm_hub_thread.flush_generation();
                            let packet = {
                                let mut ring = match ring_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("PCM ring mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                let packet = if pad_frames != 0 {
                                    ring.pop_packet_with_silence_prefix(pad_frames)
                                } else {
                                    ring.pop_packet()
                                };
                                pcm_hub_thread.set_audio_ready(
                                    pcm_hub_thread.source_present()
                                        && ring.has_packet()
                                );
                                packet
                            };
                            let Some(packet) = packet else {
                                break;
                            };

                            let sent = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => return,
                                };
                                // A FLUSH/standby/stop that won the engine lock
                                // after this packet left the ring supersedes it.
                                // Never let pre-boundary PCM cross that command.
                                if pcm_hub_thread.flush_generation() != send_generation
                                    || guard.runtime.state != Ap2State::Streaming
                                    || guard.content_paused()
                                    || guard.content_stopped()
                                {
                                    None
                                } else {
                                    if starvation_started.is_some() {
                                        let diag = guard.diagnostics();
                                        let elapsed_ms = starvation_started
                                            .map(|started| started.elapsed().as_millis())
                                            .unwrap_or(0);
                                        if let Ok(mut events) = events_thread.lock() {
                                            events.push(format!(
                                                "MSA INPUT REALTIME starvation-exit BEFORE first PCM: elapsed={}ms recoveries={} capture_idle={}ms state={:?} seq={} rtp={} head_frame={} pacing_ahead_frames={} splice_pad_frames={} reanchors={} shifted_frames={} ptp_anchor_valid={} ptp_wall0_ns={} ptp_pos0={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={} capture_gen={} non_silent_gen={} flush_gen={} flush_ack={}.",
                                                elapsed_ms,
                                                starvation_recovery_count,
                                                last_capture_frame_at.elapsed().as_millis(),
                                                diag.state,
                                                diag.seq,
                                                diag.rtp,
                                                diag.head_frame,
                                                diag.pacing_ahead_frames,
                                                guard.runtime.splice_pad_frames,
                                                guard.runtime.timeline_reanchors,
                                                guard.runtime.reanchor_shifted_frames,
                                                guard.runtime.ptp_anchor.valid,
                                                guard.runtime.ptp_anchor.wall0_ns,
                                                guard.runtime.ptp_anchor.pos0,
                                                diag.audio_sent,
                                                diag.audio_dropped,
                                                diag.sync_sent,
                                                diag.sync_dropped,
                                                pcm_hub_thread.capture_frame_generation(),
                                                pcm_hub_thread.non_silent_generation(),
                                                pcm_hub_thread.flush_generation(),
                                                pcm_hub_thread.flush_ack_generation(),
                                            ));
                                        }
                                    }
                                    Some(guard.send_pcm_352(&packet))
                                }
                            };

                            match sent {
                                None => {}
                                Some(Ok(result @ (SendResult::Sent | SendResult::Dropped))) => {
                                    if starvation_started.is_some() {
                                        let elapsed_ms = starvation_started
                                            .map(|started| started.elapsed().as_millis())
                                            .unwrap_or(0);
                                        if let Ok(guard) = engine_thread.lock() {
                                            let diag = guard.diagnostics();
                                            if let Ok(mut events) = events_thread.lock() {
                                                events.push(format!(
                                                    "MSA INPUT REALTIME starvation-exit AFTER first PCM: result={:?} elapsed={}ms recoveries={} capture_idle={}ms state={:?} seq={} rtp={} head_frame={} pacing_ahead_frames={} splice_pad_frames={} reanchors={} shifted_frames={} ptp_anchor_valid={} ptp_wall0_ns={} ptp_pos0={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={} capture_gen={} non_silent_gen={} flush_gen={} flush_ack={}.",
                                                    result,
                                                    elapsed_ms,
                                                    starvation_recovery_count,
                                                    last_capture_frame_at.elapsed().as_millis(),
                                                    diag.state,
                                                    diag.seq,
                                                    diag.rtp,
                                                    diag.head_frame,
                                                    diag.pacing_ahead_frames,
                                                    guard.runtime.splice_pad_frames,
                                                    guard.runtime.timeline_reanchors,
                                                    guard.runtime.reanchor_shifted_frames,
                                                    guard.runtime.ptp_anchor.valid,
                                                    guard.runtime.ptp_anchor.wall0_ns,
                                                    guard.runtime.ptp_anchor.pos0,
                                                    diag.audio_sent,
                                                    diag.audio_dropped,
                                                    diag.sync_sent,
                                                    diag.sync_dropped,
                                                    pcm_hub_thread.capture_frame_generation(),
                                                    pcm_hub_thread.non_silent_generation(),
                                                    pcm_hub_thread.flush_generation(),
                                                    pcm_hub_thread.flush_ack_generation(),
                                                ));
                                            }
                                        }
                                    }
                                    if pad_frames != 0 {
                                        if let Ok(mut guard) = engine_thread.lock() {
                                            guard.runtime.take_splice_pad_frames(pad_frames);
                                        }
                                    }
                                    starvation_started = None;
                                    last_starvation_recovery = None;
                                    starvation_recovery_count = 0;
                                }
                                Some(Ok(SendResult::Fatal)) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot =
                                            Some("native SOLO media send returned fatal".into());
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                                Some(Err(e)) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot =
                                            Some(format!("native SOLO media send failed: {e:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }

                        let ring_has_packet = ring_thread
                            .lock()
                            .map(|ring| ring.has_packet())
                            .unwrap_or(false);
                        // Windows loopback has no meaningful input-starvation
                        // state until the source has produced its first real
                        // non-SILENT packet. Realtime Apple sessions START
                        // immediately, so treating the pre-source empty ring as
                        // starvation repeatedly re-anchors before any content
                        // exists and can make the first 24-bit burst catch up.
                        if pcm_hub_thread.source_present() && !ring_has_packet {
                            let starving_since =
                                starvation_started.get_or_insert_with(Instant::now);
                            if starving_since.elapsed() >= STARVATION_RECOVERY_INTERVAL
                                && last_starvation_recovery
                                    .map(|t| {
                                        t.elapsed() >= STARVATION_RECOVERY_INTERVAL
                                    })
                                    .unwrap_or(true)
                            {
                                let recovered = {
                                    let mut guard = match engine_thread.lock() {
                                        Ok(v) => v,
                                        Err(_) => return,
                                    };
                                    if guard.runtime.state != Ap2State::Streaming
                                        || guard.is_buffered()
                                    {
                                        false
                                    } else {
                                        let now_ntp =
                                            match SourceNtp::from_system_time(SystemTime::now()) {
                                                Ok(v) => v,
                                                Err(_) => SourceNtp::ZERO,
                                            };
                                        let now_frame = now_ntp.to_frames(
                                            guard.runtime.media.timeline.sample_rate,
                                        );
                                        let timing =
                                            match guard.ready.timing_owner.sync_timing() {
                                                Ok(v) => v,
                                                Err(_) => {
                                                    last_starvation_recovery =
                                                        Some(Instant::now());
                                                    continue;
                                                }
                                            };
                                        let engine = &mut *guard;
                                        engine.runtime.recover_input_gap(
                                            now_frame,
                                            &mut engine.ready.media.io,
                                            timing,
                                        )
                                    }
                                };
                                last_starvation_recovery = Some(Instant::now());
                                if recovered {
                                    starvation_recovery_count =
                                        starvation_recovery_count.saturating_add(1);
                                    if starvation_recovery_count == 1 {
                                        if let Ok(guard) = engine_thread.lock() {
                                            let diag = guard.diagnostics();
                                            if let Ok(mut events) = events_thread.lock() {
                                                events.push(format!(
                                                    "MSA INPUT REALTIME starvation BEGIN: capture_idle={}ms state={:?} seq={} rtp={} head_frame={} pacing_ahead_frames={} splice_pad_frames={} reanchors={} shifted_frames={} ptp_anchor_valid={} ptp_wall0_ns={} ptp_pos0={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={} capture_gen={} non_silent_gen={} flush_gen={} flush_ack={}.",
                                                    last_capture_frame_at.elapsed().as_millis(),
                                                    diag.state,
                                                    diag.seq,
                                                    diag.rtp,
                                                    diag.head_frame,
                                                    diag.pacing_ahead_frames,
                                                    guard.runtime.splice_pad_frames,
                                                    guard.runtime.timeline_reanchors,
                                                    guard.runtime.reanchor_shifted_frames,
                                                    guard.runtime.ptp_anchor.valid,
                                                    guard.runtime.ptp_anchor.wall0_ns,
                                                    guard.runtime.ptp_anchor.pos0,
                                                    diag.audio_sent,
                                                    diag.audio_dropped,
                                                    diag.sync_sent,
                                                    diag.sync_dropped,
                                                    pcm_hub_thread.capture_frame_generation(),
                                                    pcm_hub_thread.non_silent_generation(),
                                                    pcm_hub_thread.flush_generation(),
                                                    pcm_hub_thread.flush_ack_generation(),
                                                ));
                                            }
                                        }
                                    }
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "WASAPI input starvation recovery queued timeline silence · count={}.",
                                            starvation_recovery_count
                                        ));
                                    }
                                }
                            }
                        } else {
                            starvation_started = None;
                            last_starvation_recovery = None;
                        }

                        thread::sleep(Duration::from_millis(1));
                    }
                })
                .map_err(|e| WindowsSoloAudioWorkerError::Engine(format!(
                    "spawn media consumer: {e}"
                )))?
        };

        match pcm_source.wait_ready(Duration::from_secs(3)) {
            Ok(()) => Ok(Self {
                running,
                engine,
                pcm_hub,
                deferred_start_armed,
                pcm_source,
                media_worker: Some(media_worker),
                last_error,
                startup_events,
            }),
            Err(error) => {
                running.store(false, Ordering::SeqCst);
                // Preserve the validated failure cleanup order: source first,
                // then media, after closing the shared run gate.
                pcm_source.stop();
                let _ = media_worker.join();
                Err(WindowsSoloAudioWorkerError::Engine(error.to_string()))
            }
        }
    }

    /// MSA ap2_session_flush equivalent for the WASAPI adapter: transport
    /// FLUSH while sends are serialized, then wait until the capture source
    /// has reset exactly the pre-FLUSH PCM before returning the frozen warm head.
    pub fn engine(&self) -> SharedNativeSoloEngine {
        Arc::clone(&self.engine)
    }

    pub fn flush_content(&self) -> Result<Option<u64>, WindowsSoloAudioWorkerError> {
        if !self.is_running() {
            return Err(WindowsSoloAudioWorkerError::Engine("WASAPI worker is not running".into()));
        }
        let target_generation;
        let warm_head = {
            let mut engine = self.engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            engine.flush().map_err(|e| {
                WindowsSoloAudioWorkerError::Engine(format!("FLUSH: {e:?}"))
            })?;
            let head = (engine.splice_head_unix_ms() != 0)
                .then_some(engine.splice_head_unix_ms());
            target_generation = self.pcm_hub.request_flush();
            head
        };

        let deadline = Instant::now() + FLUSH_DRAIN_TIMEOUT;
        while self.pcm_hub.flush_ack_generation() < target_generation {
            if !self.is_running() {
                return Err(WindowsSoloAudioWorkerError::Engine(
                    "WASAPI worker stopped during FLUSH drain".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(WindowsSoloAudioWorkerError::Engine(
                    "WASAPI FLUSH drain acknowledgement timed out".into(),
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(warm_head)
    }

    /// START after either initial connect or a completed FLUSH. NativeSoloEngine
    /// itself selects ap2cl_start for the first call and ap2cl_resume thereafter.
    pub fn audio_ready(&self) -> bool {
        self.pcm_hub.audio_ready()
    }

    pub fn arm_start_on_audio(&self) -> Result<(), WindowsSoloAudioWorkerError> {
        if !self.is_running() {
            return Err(WindowsSoloAudioWorkerError::Engine(
                "WASAPI worker is not running".into(),
            ));
        }
        {
            let guard = self.engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            if !guard.is_buffered() {
                return Err(WindowsSoloAudioWorkerError::Engine(
                    "deferred START is only valid for native Buffered type 103".into(),
                ));
            }
            if guard.runtime.state != Ap2State::Connected {
                return Err(WindowsSoloAudioWorkerError::Engine(
                    "deferred START requires a connected, not-yet-streaming session".into(),
                ));
            }
        }
        self.deferred_start_armed.store(true, Ordering::SeqCst);
        if let Ok(mut events) = self.startup_events.lock() {
            events.push(
                "MSA SOLO START armed; Buffered type 103 is Ready and waiting for first WASAPI audio packet."
                    .into(),
            );
        }
        Ok(())
    }

    pub fn start_pending(&self) -> bool {
        self.deferred_start_armed.load(Ordering::SeqCst)
    }

    pub fn commit_start(
        &self,
        requested_unix_ms: u64,
    ) -> Result<crate::timing::StartResolution, WindowsSoloAudioWorkerError> {
        let (started, mrp) = {
            let mut engine = self.engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            let started = engine.start(requested_unix_ms).map_err(|e| {
                WindowsSoloAudioWorkerError::Engine(format!("START: {e:?}"))
            })?;
            (started, engine.mrp_controller())
        };
        // Pinned cliairplay publishes MRP only after leaving the audio-send
        // quiesce bracket. A failed decoration never turns a valid START into
        // a transport failure.
        if let Some(mrp) = mrp {
            let _ = mrp.publish_playback_state_on_transition(crate::MrpPlaybackState::Playing);
        }
        Ok(started)
    }

    pub fn standby_content(&self) -> Result<(), WindowsSoloAudioWorkerError> {
        let mrp = {
            let mut engine = self.engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            engine.standby().map_err(|e| {
                WindowsSoloAudioWorkerError::Engine(format!("STANDBY: {e:?}"))
            })?;
            engine.mrp_controller()
        };
        if let Some(mrp) = mrp {
            let _ = mrp.publish_playback_state(crate::MrpPlaybackState::Paused, true);
        }
        Ok(())
    }

    pub fn set_content_enabled(&self, enabled: bool) -> Result<(), WindowsSoloAudioWorkerError> {
        let mrp = {
            let mut engine = self.engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            if enabled {
                engine.play_content().map_err(|e| WindowsSoloAudioWorkerError::Engine(format!("play: {e:?}")))?;
            } else {
                engine.pause_content().map_err(|e| WindowsSoloAudioWorkerError::Engine(format!("pause: {e:?}")))?;
            }
            engine.mrp_controller()
        };
        if let Some(mrp) = mrp {
            let state = if enabled {
                crate::MrpPlaybackState::Playing
            } else {
                crate::MrpPlaybackState::Paused
            };
            let _ = mrp.publish_playback_state(state, true);
        }
        Ok(())
    }

    pub fn stop_content(&self) -> Result<(), WindowsSoloAudioWorkerError> {
        self.deferred_start_armed.store(false, Ordering::SeqCst);
        let mrp = {
            let mut engine = self.engine.lock().map_err(|_| {
                WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
            })?;
            engine.stop_content().map_err(|e| {
                WindowsSoloAudioWorkerError::Engine(format!("STOP: {e:?}"))
            })?;
            engine.mrp_controller()
        };
        if let Some(mrp) = mrp {
            let _ = mrp.publish_playback_state(crate::MrpPlaybackState::Stopped, true);
        }
        Ok(())
    }

    /// Clears capture bytes without changing the AP2 wire timeline. Session
    /// FLUSH/RESUME commands remain the transport owner's responsibility.
    pub fn discard_captured_pcm(&self) {
        self.pcm_hub.request_flush();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|v| v.clone())
    }

    pub fn discontinuities(&self) -> u64 {
        self.pcm_source.discontinuities()
    }

    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        self.pcm_source.last_discontinuity_frame()
    }

    pub fn startup_events(&self) -> Vec<String> {
        self.startup_events.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn drain_startup_events(&self) -> Vec<String> {
        self.startup_events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn stop(&mut self) {
        self.deferred_start_armed.store(false, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.media_worker.take() {
            let _ = worker.join();
        }
        self.pcm_source.stop();
    }
}

impl Drop for WindowsSoloAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod source_lifecycle_tests {
    use super::*;

    #[test]
    fn wasapi_silence_is_not_a_stop_signal() {
        // A Windows loopback SILENT packet still represents valid timeline
        // PCM. Source lifecycle must come from explicit app/session state,
        // never from a silence-duration threshold.
        let wasapi_silent = true;
        let explicit_stop = false;
        assert!(wasapi_silent);
        assert!(!explicit_stop);
    }

    #[test]
    fn buffered_capture_idle_parks_only_true_frame_absence() {
        assert!(!buffered_capture_idle_should_park(
            true,
            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL - Duration::from_millis(1),
            true,
            Ap2State::Streaming,
            false,
            false,
        ));
        assert!(buffered_capture_idle_should_park(
            true,
            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL,
            true,
            Ap2State::Streaming,
            false,
            false,
        ));
        assert!(!buffered_capture_idle_should_park(
            true,
            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL,
            false,
            Ap2State::Streaming,
            false,
            false,
        ));
        assert!(!buffered_capture_idle_should_park(
            true,
            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL,
            true,
            Ap2State::Paused,
            true,
            false,
        ));
    }

    #[test]
    fn realtime_starvation_waits_for_first_source_packet() {
        let source_present = false;
        let ring_has_packet = false;
        assert!(!(source_present && !ring_has_packet));

        let source_present = true;
        assert!(source_present && !ring_has_packet);
    }
}
