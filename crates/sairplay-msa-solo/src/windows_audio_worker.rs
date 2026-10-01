//! Windows WASAPI -> native AP2 SOLO media worker.
//! The worker owns the COM/WASAPI capture apartment; the engine stays behind
//! a shared mutex so lifecycle commands can serialize with media sends.

use crate::{
    ap2::Ap2State,
    native_media::SendResult,
    time_domain::SourceNtp,
    Ap2AudioFormat, NativeSoloEngine, Pcm352Chunker, SoloClockReadinessState,
    WasapiLoopbackCapture, WasapiLoopbackError,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const AIRPLAY_CLOCK_READY_TIMEOUT: Duration = Duration::from_millis(2500);
pub const STARVATION_RECOVERY_INTERVAL: Duration = Duration::from_millis(250);
/// Passive Windows loopback has no explicit player pause command. Require two
/// MSA starvation intervals without a non-SILENT source packet before mapping
/// source disappearance to Buffered STANDBY.
pub const BUFFERED_SOURCE_IDLE_PARK_INTERVAL: Duration = Duration::from_millis(500);
pub const FLUSH_DRAIN_TIMEOUT: Duration = Duration::from_millis(2000);
const DEFERRED_START_LEAD_MS: u64 = 400;
const DEFERRED_CLOCK_READY_LEAD_MS: u64 = 500;

pub type SharedNativeSoloEngine = Arc<Mutex<NativeSoloEngine>>;

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
    flush_generation: Arc<AtomicU64>,
    flush_ack_generation: Arc<AtomicU64>,
    audio_ready: Arc<AtomicBool>,
    deferred_start_armed: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
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
        let flush_generation = Arc::new(AtomicU64::new(0));
        let flush_ack_generation = Arc::new(AtomicU64::new(0));
        let audio_ready = Arc::new(AtomicBool::new(false));
        let deferred_start_armed = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));
        let discontinuities = Arc::new(AtomicU64::new(0));
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));

        let running_thread = Arc::clone(&running);
        let flush_thread = Arc::clone(&flush_generation);
        let flush_ack_thread = Arc::clone(&flush_ack_generation);
        let audio_ready_thread = Arc::clone(&audio_ready);
        let deferred_start_thread = Arc::clone(&deferred_start_armed);
        let error_thread = Arc::clone(&last_error);
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_thread = Arc::clone(&last_discontinuity_frame);
        let events_thread = Arc::clone(&startup_events);
        let engine_thread = Arc::clone(&engine);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("sairplay-msa-wasapi".into())
            .spawn(move || {
                let mut capture = match WasapiLoopbackCapture::open_default_for_format(audio_format) {
                    Ok(v) => {
                        if let Ok(mut events) = events_thread.lock() {
                            events.push(format!("MSA INPUT {}.", v.format_summary()));
                        }
                        let _ = ready_tx.send(Ok(()));
                        v
                    }
                    Err(e) => {
                        let message = e.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        if let Ok(mut slot) = error_thread.lock() { *slot = Some(message); }
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let mut chunker =
                    Pcm352Chunker::new_with_bytes_per_frame(audio_format.input_bytes_per_frame());
                let mut captured_frames_total = 0u64;
                // Cold-start adapter invariant: WASAPI engine-silent packets are
                // not equivalent to MSA stdin audio_present. Latch only after
                // the first packet not marked AUDCLNT_BUFFERFLAGS_SILENT.
                let mut source_present = false;
                let mut local_flush_generation = flush_thread.load(Ordering::SeqCst);
                let mut starvation_started: Option<Instant> = None;
                let mut last_starvation_recovery: Option<Instant> = None;
                let mut deferred_audio_seen: Option<Instant> = None;
                let mut buffered_source_idle_since: Option<Instant> = None;

                while running_thread.load(Ordering::SeqCst) {
                    // Exact cliairplay outer-loop health gate: MediaRemote
                    // reverse-event health is part of the control verdict,
                    // even when RTSP/media sockets themselves are still alive.
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

                    let generation = flush_thread.load(Ordering::SeqCst);
                    if generation != local_flush_generation {
                        chunker.clear();
                        capture.reset_conversion();
                        local_flush_generation = generation;
                        starvation_started = None;
                        last_starvation_recovery = None;
                        buffered_source_idle_since = None;
                        audio_ready_thread.store(false, Ordering::SeqCst);
                        flush_ack_thread.store(generation, Ordering::SeqCst);
                    }

                    let report = match capture.drain_into(&mut chunker) {
                        Ok(v) => v,
                        Err(e) => {
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(format!("WASAPI capture failed: {e}"));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    };

                    if report.discontinuities != 0 {
                        discontinuities_thread.fetch_add(
                            report.discontinuities,
                            Ordering::SeqCst,
                        );
                        if let Some(offset) = report.discontinuity_frame_offset {
                            last_discontinuity_thread.store(
                                captured_frames_total.saturating_add(offset),
                                Ordering::SeqCst,
                            );
                        }
                    }
                    captured_frames_total =
                        captured_frames_total.saturating_add(report.frames as u64);

                    // Pinned MSA waits for actual source bytes before START.
                    // Windows shared-loopback can emit engine-generated SILENT
                    // packets while no application is playing; those packets
                    // must not satisfy audio_present. Preserve stable's adapter
                    // boundary: drop pre-source engine silence, reset conversion
                    // history, and wait indefinitely for the first non-SILENT
                    // WASAPI packet. This deliberately does not inspect sample
                    // amplitude, so a real digital-zero source remains valid.
                    if !source_present {
                        if report.first_non_silent_frame_offset.is_some() {
                            source_present = true;
                            if let Ok(mut events) = events_thread.lock() {
                                events.push(
                                    "MSA INPUT source-present: first non-SILENT WASAPI packet."
                                        .into(),
                                );
                            }
                        } else {
                            chunker.clear();
                            capture.reset_conversion();
                            audio_ready_thread.store(false, Ordering::SeqCst);
                            deferred_audio_seen = None;
                            if report.frames == 0 {
                                thread::sleep(Duration::from_millis(1));
                            }
                            continue;
                        }
                    }

                    // MSA's persistent input ring is max(4 seconds, 1 MiB).
                    // WASAPI cannot backpressure the system mixer, so preserve
                    // the oldest resident bytes and discard only new excess.
                    let byte_rate = audio_format.sample_rate as usize
                        * audio_format.input_bytes_per_frame();
                    let ring_capacity = (byte_rate.saturating_mul(4)).max(1 << 20);
                    let _ = chunker.truncate_pending(ring_capacity);
                    if chunker.has_packet() {
                        audio_ready_thread.store(true, Ordering::SeqCst);
                    }

                    // Windows loopback differs from MSA's ffmpeg/stdin source:
                    // a Buffered type-103 receiver must not be anchored before
                    // source-present PCM exists and a complete packet is retained.
                    // Engine-silent WASAPI buffers were rejected above. Keep the
                    // first source packet resident in the chunker while the
                    // receiver clock projection settles.
                    if deferred_start_thread.load(Ordering::SeqCst) && chunker.has_packet() {
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
                                let uses_ptp = guard.uses_ptp();
                                let readiness = guard.clock_readiness();
                                let have_projection = uses_ptp
                                    && matches!(
                                        readiness.state,
                                        SoloClockReadinessState::Probing
                                            | SoloClockReadinessState::Ready
                                    )
                                    && readiness.ready_at_unix_ms != 0;
                                let projection_timeout =
                                    first_audio_at.elapsed() >= AIRPLAY_CLOCK_READY_TIMEOUT;

                                if uses_ptp && !have_projection && !projection_timeout {
                                    None
                                } else {
                                    let ready_at = if have_projection {
                                        readiness.ready_at_unix_ms
                                    } else {
                                        0
                                    };
                                    let buffered_connected_now =
                                        match guard.ensure_buffered_media_connected() {
                                            Ok(v) => v,
                                            Err(e) => {
                                                if let Ok(mut slot) = error_thread.lock() {
                                                    *slot = Some(format!(
                                                        "deferred native SOLO media connect failed: {e:?}"
                                                    ));
                                                }
                                                running_thread.store(false, Ordering::SeqCst);
                                                return;
                                            }
                                        };

                                    let mut requested =
                                        now_unix_ms.saturating_add(DEFERRED_START_LEAD_MS);
                                    if ready_at != 0 {
                                        requested = requested.max(
                                            ready_at
                                                .saturating_add(DEFERRED_CLOCK_READY_LEAD_MS),
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
                        )) = start_attempt {
                            deferred_start_thread.store(false, Ordering::SeqCst);
                            if let Ok(mut events) = events_thread.lock() {
                                events.push(
                                    "MSA SOLO AUDIO first packet present; committing deferred Buffered START."
                                        .into(),
                                );
                                if buffered_connected_now {
                                    events.push(
                                        "MSA SOLO BUFFERED data TCP connected at source-present START boundary."
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

                    // Buffered type-103 has no realtime starvation re-anchor in
                    // pinned MSA. MSA parks it explicitly (rate=0 +
                    // FLUSHBUFFERED -> CONNECTED) when the source is stopped,
                    // then resumes on a fresh rate=1 anchor. Windows loopback
                    // has no explicit source STOP event, so adapt only the
                    // engine's source-presence signal: two starvation windows
                    // with no non-SILENT WASAPI packet means the passive source
                    // has gone idle. Digital-zero content remains source-present
                    // because it arrives in a non-SILENT WASAPI packet.
                    let source_packet_present =
                        report.first_non_silent_frame_offset.is_some();
                    if source_packet_present {
                        buffered_source_idle_since = None;
                    } else {
                        let buffered_streaming = engine_thread
                            .lock()
                            .map(|guard| {
                                guard.is_buffered()
                                    && guard.runtime.state == Ap2State::Streaming
                                    && !guard.content_paused()
                                    && !guard.content_stopped()
                            })
                            .unwrap_or(false);
                        if buffered_streaming {
                            let idle_since =
                                buffered_source_idle_since.get_or_insert_with(Instant::now);
                            if idle_since.elapsed() >= BUFFERED_SOURCE_IDLE_PARK_INTERVAL {
                                let park_result = {
                                    let mut guard = match engine_thread.lock() {
                                        Ok(v) => v,
                                        Err(_) => {
                                            if let Ok(mut slot) = error_thread.lock() {
                                                *slot = Some(
                                                    "native SOLO engine mutex poisoned".into(),
                                                );
                                            }
                                            running_thread.store(false, Ordering::SeqCst);
                                            return;
                                        }
                                    };
                                    if guard.is_buffered()
                                        && guard.runtime.state == Ap2State::Streaming
                                    {
                                        let result = guard.standby();
                                        let mrp = guard.mrp_controller();
                                        Some((result, mrp))
                                    } else {
                                        None
                                    }
                                };

                                if let Some((result, mrp)) = park_result {
                                    if let Err(e) = result {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some(format!(
                                                "Buffered source-idle STANDBY failed: {e:?}"
                                            ));
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }

                                    // Drop any engine-generated silence or
                                    // pre-park tail. MSA standby discards the
                                    // parked receiver queue before the next
                                    // source is admitted.
                                    chunker.clear();
                                    capture.reset_conversion();
                                    source_present = false;
                                    audio_ready_thread.store(false, Ordering::SeqCst);
                                    deferred_audio_seen = None;
                                    buffered_source_idle_since = None;
                                    starvation_started = None;
                                    last_starvation_recovery = None;
                                    deferred_start_thread.store(true, Ordering::SeqCst);

                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA SOLO BUFFERED source idle for >= {}ms; rate-0 STANDBY + FLUSHBUFFERED completed, waiting for source resume.",
                                            BUFFERED_SOURCE_IDLE_PARK_INTERVAL.as_millis(),
                                        ));
                                    }
                                    if let Some(mrp) = mrp {
                                        let _ = mrp.publish_playback_state(
                                            crate::MrpPlaybackState::Paused,
                                            true,
                                        );
                                    }
                                    thread::sleep(Duration::from_millis(1));
                                    continue;
                                }
                            }
                        } else {
                            buffered_source_idle_since = None;
                        }
                    }

                    let content_paused_or_stopped = engine_thread
                        .lock()
                        .map(|guard| guard.content_paused() || guard.content_stopped())
                        .unwrap_or(true);
                    if content_paused_or_stopped {
                        // The transport may intentionally remain STREAMING on
                        // the splice path. Feed contiguous silence there, but
                        // never turn captured system PCM into content while the
                        // single authoritative engine state says content is paused.
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
                                        let silence =
                                            vec![0u8; 352 * audio_format.input_bytes_per_frame()];
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

                    // Pinned ap2_session ownership: the reader only announces
                    // audio readiness. START is committed exclusively by the
                    // command/session path (commit_start), never by capture.
                    let transport_streaming = engine_thread
                        .lock()
                        .map(|guard| guard.runtime.state == Ap2State::Streaming)
                        .unwrap_or(false);
                    if !transport_streaming {
                        if report.frames == 0 {
                            thread::sleep(Duration::from_millis(1));
                        }
                        continue;
                    }

                    // Source-order delivery-stall guard: pacing gate first,
                    // then recovery before consuming real content.
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
                                        *slot = Some(format!("native SOLO pacing failed: {e:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        };
                        if !can_accept { break; }

                        let pad_frames = {
                            let mut guard = match engine_thread.lock() {
                                Ok(v) => v,
                                Err(_) => return,
                            };
                            let now_ntp = match SourceNtp::from_system_time(SystemTime::now()) {
                                Ok(v) => v,
                                Err(e) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some(format!("clock conversion failed: {e}"));
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

                        let packet = if pad_frames != 0 {
                            match chunker.pop_packet_with_silence_prefix(pad_frames) {
                                Some(v) => v,
                                None => break,
                            }
                        } else {
                            match chunker.pop_packet() {
                                Some(v) => v,
                                None => break,
                            }
                        };

                        let sent = {
                            let mut guard = match engine_thread.lock() {
                                Ok(v) => v,
                                Err(_) => return,
                            };
                            guard.send_pcm_352(&packet)
                        };
                        match sent {
                            Ok(SendResult::Sent | SendResult::Dropped) => {
                                if pad_frames != 0 {
                                    if let Ok(mut guard) = engine_thread.lock() {
                                        guard.runtime.take_splice_pad_frames(pad_frames);
                                    }
                                }
                                starvation_started = None;
                                last_starvation_recovery = None;
                            }
                            Ok(SendResult::Fatal) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some("native SOLO media send returned fatal".into());
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                            Err(e) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!("native SOLO media send failed: {e:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                    }

                    // MSA waits for 250 ms read intervals before declaring
                    // input starvation. Buffered type-103 intentionally does
                    // no starvation recovery.
                    if !chunker.has_packet() {
                        let starving_since = starvation_started.get_or_insert_with(Instant::now);
                        if starving_since.elapsed() >= STARVATION_RECOVERY_INTERVAL
                            && last_starvation_recovery
                                .map(|t| t.elapsed() >= STARVATION_RECOVERY_INTERVAL)
                                .unwrap_or(true)
                        {
                            let recovered = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => return,
                                };
                                if guard.runtime.state != Ap2State::Streaming {
                                    false
                                } else {
                                    let now_ntp = match SourceNtp::from_system_time(SystemTime::now()) {
                                        Ok(v) => v,
                                        Err(_) => SourceNtp::ZERO,
                                    };
                                    let now_frame =
                                        now_ntp.to_frames(guard.runtime.media.timeline.sample_rate);
                                    let timing = match guard.ready.timing_owner.sync_timing() {
                                        Ok(v) => v,
                                        Err(_) => {
                                            last_starvation_recovery = Some(Instant::now());
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
                                if let Ok(mut events) = events_thread.lock() {
                                    events.push("WASAPI input starvation recovery queued timeline silence.".into());
                                }
                            }
                        }
                    } else {
                        starvation_started = None;
                        last_starvation_recovery = None;
                    }

                    if report.frames == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            })
            .map_err(|e| WindowsSoloAudioWorkerError::Engine(format!("spawn worker: {e}")))?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                running,
                engine,
                flush_generation,
                flush_ack_generation,
                audio_ready,
                deferred_start_armed,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
                startup_events,
            }),
            Ok(Err(message)) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(WindowsSoloAudioWorkerError::Engine(message))
            }
            Err(_) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(WindowsSoloAudioWorkerError::Engine(
                    "WASAPI worker did not become ready".into(),
                ))
            }
        }
    }

    /// MSA ap2_session_flush equivalent for the WASAPI adapter: transport
    /// FLUSH while sends are serialized, then wait until the capture worker
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
            target_generation = self.flush_generation.fetch_add(1, Ordering::SeqCst) + 1;
            head
        };

        let deadline = Instant::now() + FLUSH_DRAIN_TIMEOUT;
        while self.flush_ack_generation.load(Ordering::SeqCst) < target_generation {
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
        self.audio_ready.load(Ordering::SeqCst)
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
        self.flush_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|v| v.clone())
    }

    pub fn discontinuities(&self) -> u64 {
        self.discontinuities.load(Ordering::SeqCst)
    }

    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        let v = self.last_discontinuity_frame.load(Ordering::SeqCst);
        (v != u64::MAX).then_some(v)
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
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WindowsSoloAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod source_idle_tests {
    use super::*;

    #[test]
    fn buffered_idle_park_requires_two_starvation_intervals() {
        assert_eq!(
            BUFFERED_SOURCE_IDLE_PARK_INTERVAL,
            STARVATION_RECOVERY_INTERVAL + STARVATION_RECOVERY_INTERVAL
        );
    }
}
