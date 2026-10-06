#![cfg(windows)]

//! Windows PCM adapter for the concrete MSA-pinned RAOP session.
//!
//! RAOP is a first-class receiver lane and shares the same Windows PCM source
//! and bounded hub as native AP2. Transport ownership remains entirely inside
//! the pinned libraop-backed session; this worker only adapts WASAPI capture to
//! that transport and mirrors MSA's quiesce -> transport FLUSH -> local drain
//! ordering at lifecycle boundaries.

use crate::{
    Ap2AudioFormat, MsaRaopConfig, MsaRaopError, MsaRaopPcmWriter, MsaRaopSession,
    WasapiLoopbackError, WindowsPcmHub, WindowsPcmSource,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FLUSH_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const RAOP_DIAG_SUMMARY_INTERVAL: Duration = Duration::from_secs(10);
const RAOP_DIAG_STALL_THRESHOLD: Duration = Duration::from_millis(20);
// Pinned MSA/libraop needs 200 ms of feasible sender-side START lead.  MSA's
// normal FFmpeg/stdin producer can burst pre-buffered PCM before START, whereas
// WASAPI loopback is a hard realtime producer and cannot supply audio from the
// past.  Keep the receiver's negotiated render latency in front of a live
// source, then retain the same 200 ms sender-side feasibility floor.
const RAOP_LIVE_SOURCE_GUARD_MS: u64 = 200;
// AP1-only elasticity floor.  Twelve 352-frame RAOP packets are ~95.8 ms at
// 44.1 kHz, comfortably above the measured 23-31 ms resampler burst and the
// ~40 ms worst steady empty interval.  This primes the existing PCM hub only;
// it does not add another queue, alter libraop pacing, or touch native AP2.
const RAOP_RESERVOIR_PACKETS: usize = 12;
const RAOP_RESERVOIR_FRAMES: usize = RAOP_RESERVOIR_PACKETS * 352;

fn raop_reservoir_ms(sample_rate: usize) -> usize {
    if sample_rate == 0 {
        0
    } else {
        RAOP_RESERVOIR_FRAMES
            .saturating_mul(1000)
            .saturating_add(sample_rate - 1)
            / sample_rate
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

fn live_source_start_floor_unix_ms(
    now_unix_ms: u64,
    latency_frames: u32,
    sample_rate: u32,
) -> (u64, u64) {
    let receiver_latency_ms = if sample_rate == 0 {
        0
    } else {
        u64::from(latency_frames).saturating_mul(1000) / u64::from(sample_rate)
    };
    (
        now_unix_ms
            .saturating_add(receiver_latency_ms)
            .saturating_add(RAOP_LIVE_SOURCE_GUARD_MS),
        receiver_latency_ms,
    )
}

#[derive(Debug)]
pub enum WindowsRaopWorkerError {
    Session(MsaRaopError),
    Capture(WasapiLoopbackError),
    Worker(String),
}
impl fmt::Display for WindowsRaopWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(e) => write!(f, "{e}"),
            Self::Capture(e) => write!(f, "{e}"),
            Self::Worker(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for WindowsRaopWorkerError {}
impl From<MsaRaopError> for WindowsRaopWorkerError {
    fn from(v: MsaRaopError) -> Self { Self::Session(v) }
}
impl From<WasapiLoopbackError> for WindowsRaopWorkerError {
    fn from(v: WasapiLoopbackError) -> Self { Self::Capture(v) }
}

pub type SharedMsaRaopSession = Arc<Mutex<MsaRaopSession>>;

fn diagnostic_head_ahead_ms(session: &SharedMsaRaopSession) -> Option<i128> {
    let guard = session.try_lock().ok()?;
    let head_unix_ms = guard.head_audible_unix_ms();
    if head_unix_ms == 0 {
        return None;
    }
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i128;
    Some(head_unix_ms as i128 - now_unix_ms)
}

pub struct WindowsRaopAudioWorker {
    session: SharedMsaRaopSession,
    running: Arc<AtomicBool>,
    delivery_enabled: Arc<AtomicBool>,
    /// Mirrors cliairplay's g_audio_send_lock. Lifecycle commands take this
    /// gate before touching the transport so no packet can race a FLUSH/PAUSE.
    send_gate: Arc<Mutex<()>>,
    pcm_hub: WindowsPcmHub,
    pcm_source: WindowsPcmSource,
    first_start_done: Arc<AtomicBool>,
    writer_worker: Option<JoinHandle<()>>,
    health_worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
}

impl WindowsRaopAudioWorker {
    pub fn connect(config: MsaRaopConfig) -> Result<Self, WindowsRaopWorkerError> {
        let session = Arc::new(Mutex::new(MsaRaopSession::connect(config)?));
        Self::start(session)
    }

    pub fn start(session: SharedMsaRaopSession) -> Result<Self, WindowsRaopWorkerError> {
        let (pcm_writer, ready) = {
            let guard = session
                .lock()
                .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            (guard.pcm_writer(), guard.ready())
        };
        let audio_format = Ap2AudioFormat {
            sample_rate: ready.sample_rate,
            bit_depth: ready.bit_depth,
            channels: ready.channels,
        };

        let running = Arc::new(AtomicBool::new(true));
        let delivery_enabled = Arc::new(AtomicBool::new(false));
        let send_gate = Arc::new(Mutex::new(()));
        let pcm_hub = WindowsPcmHub::new(audio_format);
        let first_start_done = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));

        // One transport-agnostic Windows producer for both AP1/RAOP and AP2.
        // It owns WASAPI capture and the bounded PCM hub; receiver network I/O
        // remains on the consumer below and can never block capture.
        let mut pcm_source = WindowsPcmSource::spawn(
            audio_format,
            pcm_hub.clone(),
            Arc::clone(&running),
            Arc::clone(&last_error),
            Arc::clone(&startup_events),
            None,
        )
        .map_err(|e| WindowsRaopWorkerError::Worker(e.to_string()))?;

        let running_w = Arc::clone(&running);
        let enabled_w = Arc::clone(&delivery_enabled);
        let gate_w = Arc::clone(&send_gate);
        let session_w = Arc::clone(&session);
        let hub_w = pcm_hub.clone();
        let ring_w = pcm_hub.ring();
        let error_w = Arc::clone(&last_error);
        let events_w = Arc::clone(&startup_events);
        let bytes_per_frame_w = audio_format.input_bytes_per_frame().max(1);
        let sample_rate_w = audio_format.sample_rate.max(1) as usize;
        let reservoir_ms_w = raop_reservoir_ms(sample_rate_w);
        let writer_worker = match thread::Builder::new()
            .name("msa-raop-writer".into())
            .spawn(move || {
                let mut sent_packets_total = 0u64;
                let mut starvation_events_total = 0u64;
                let mut slow_writes_total = 0u64;
                let mut last_summary = Instant::now();
                let mut starvation_started: Option<Instant> = None;
                let mut starvation_capture_generation = 0u64;
                let mut starvation_reported = false;
                let mut last_send_done: Option<Instant> = None;
                let mut max_starvation_ms = 0u128;
                let mut max_write_ms = 0u128;
                let mut max_send_gap_ms = 0u128;
                let mut min_queue_frames = usize::MAX;
                let mut max_queue_frames = 0usize;
                let mut last_capture_generation = hub_w.capture_frame_generation();
                let mut last_capture_progress = Instant::now();
                let mut reservoir_primed = false;
                let mut reservoir_wait_started: Option<Instant> = None;

                while running_w.load(Ordering::SeqCst) {
                    let capture_generation = hub_w.capture_frame_generation();
                    if capture_generation != last_capture_generation {
                        last_capture_generation = capture_generation;
                        last_capture_progress = Instant::now();
                    }

                    if !enabled_w.load(Ordering::SeqCst) {
                        starvation_started = None;
                        starvation_capture_generation = last_capture_generation;
                        starvation_reported = false;
                        reservoir_primed = false;
                        reservoir_wait_started = None;
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }

                    // A live WASAPI producer arrives in short resampler bursts.
                    // Prime a small AP1-only reservoir before the first packet
                    // after each lifecycle boundary.  Do this outside send_gate
                    // so FLUSH/PAUSE/STOP can never wait on local buffering.
                    if !reservoir_primed {
                        if !hub_w.source_present() {
                            reservoir_wait_started = None;
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        let pending_frames = match ring_w.lock() {
                            Ok(ring) => ring.pending_bytes() / bytes_per_frame_w,
                            Err(_) => {
                                if let Ok(mut slot) = error_w.lock() {
                                    *slot = Some("RAOP PCM ring mutex poisoned".into());
                                }
                                running_w.store(false, Ordering::SeqCst);
                                break;
                            }
                        };
                        if pending_frames < RAOP_RESERVOIR_FRAMES {
                            reservoir_wait_started.get_or_insert_with(Instant::now);
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        let waited_ms = reservoir_wait_started
                            .take()
                            .map(|started| started.elapsed().as_millis())
                            .unwrap_or(0);
                        reservoir_primed = true;
                        if let Ok(mut events) = events_w.lock() {
                            events.push(format!(
                                "MSA RAOP RESERVOIR primed target={}f/{}ms queued={}f waited={}ms; AP1-only elasticity active.",
                                RAOP_RESERVOIR_FRAMES,
                                reservoir_ms_w,
                                pending_frames,
                                waited_ms,
                            ));
                        }
                    }

                    // The gate is the Windows equivalent of MSA's
                    // g_audio_send_lock: once a lifecycle command owns it, no
                    // old packet can enter libraop until that boundary ends.
                    let gate_wait_started = Instant::now();
                    let _gate = match gate_w.lock() {
                        Ok(v) => v,
                        Err(_) => {
                            if let Ok(mut slot) = error_w.lock() {
                                *slot = Some("RAOP send gate poisoned".into());
                            }
                            running_w.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let gate_wait = gate_wait_started.elapsed();
                    if !enabled_w.load(Ordering::SeqCst) {
                        continue;
                    }
                    if gate_wait >= RAOP_DIAG_STALL_THRESHOLD {
                        if let Ok(mut events) = events_w.lock() {
                            events.push(format!(
                                "MSA RAOP DIAG SEND-GATE stall={}ms capture_idle={}ms head_ahead_ms={:?}.",
                                gate_wait.as_millis(),
                                last_capture_progress.elapsed().as_millis(),
                                diagnostic_head_ahead_ms(&session_w),
                            ));
                        }
                    }

                    let (packet, pending_before_bytes) = match ring_w.lock() {
                        Ok(mut ring) => {
                            let pending = ring.pending_bytes();
                            (ring.pop_packet(), pending)
                        }
                        Err(_) => {
                            if let Ok(mut slot) = error_w.lock() {
                                *slot = Some("RAOP PCM ring mutex poisoned".into());
                            }
                            running_w.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let pending_before_frames = pending_before_bytes / bytes_per_frame_w;
                    if hub_w.source_present() {
                        min_queue_frames = min_queue_frames.min(pending_before_frames);
                        max_queue_frames = max_queue_frames.max(pending_before_frames);
                    }

                    let Some(packet) = packet else {
                        drop(_gate);
                        if hub_w.source_present() {
                            if starvation_started.is_none() {
                                starvation_started = Some(Instant::now());
                                starvation_capture_generation = last_capture_generation;
                            }
                            let empty_for = starvation_started
                                .as_ref()
                                .map(Instant::elapsed)
                                .unwrap_or_default();
                            max_starvation_ms = max_starvation_ms.max(empty_for.as_millis());
                            if empty_for >= RAOP_DIAG_STALL_THRESHOLD && !starvation_reported {
                                starvation_reported = true;
                                starvation_events_total = starvation_events_total.saturating_add(1);
                                if let Ok(mut events) = events_w.lock() {
                                    events.push(format!(
                                        "MSA RAOP DIAG PCM-STARVATION empty={}ms queue=0f capture_idle={}ms capture_gen={} events_total={} head_ahead_ms={:?}.",
                                        empty_for.as_millis(),
                                        last_capture_progress.elapsed().as_millis(),
                                        last_capture_generation,
                                        starvation_events_total,
                                        diagnostic_head_ahead_ms(&session_w),
                                    ));
                                }
                            }
                        } else {
                            starvation_started = None;
                            starvation_capture_generation = last_capture_generation;
                            starvation_reported = false;
                        }
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    };

                    if let Some(started) = starvation_started.take() {
                        let empty_elapsed = started.elapsed();
                        max_starvation_ms = max_starvation_ms.max(empty_elapsed.as_millis());
                        if starvation_reported {
                            let capture_generation_now = hub_w.capture_frame_generation();
                            let capture_gen_delta = capture_generation_now
                                .saturating_sub(starvation_capture_generation);
                            if capture_generation_now != last_capture_generation {
                                last_capture_generation = capture_generation_now;
                                last_capture_progress = Instant::now();
                            }
                            let queue_ms = pending_before_frames.saturating_mul(1000) / sample_rate_w;
                            if let Ok(mut events) = events_w.lock() {
                                events.push(format!(
                                    "MSA RAOP DIAG STARVATION-RECOVER empty={}ms capture_gen_delta={} capture_idle={}ms queue_after={}f/{}ms head_ahead_ms={:?}.",
                                    empty_elapsed.as_millis(),
                                    capture_gen_delta,
                                    last_capture_progress.elapsed().as_millis(),
                                    pending_before_frames,
                                    queue_ms,
                                    diagnostic_head_ahead_ms(&session_w),
                                ));
                            }
                        }
                    }
                    starvation_reported = false;

                    let capture_generation_before_write = hub_w.capture_frame_generation();
                    let write_started = Instant::now();
                    if let Err(e) = pcm_writer.write_packet(&packet) {
                        if let Ok(mut slot) = error_w.lock() {
                            *slot = Some(e.to_string());
                        }
                        running_w.store(false, Ordering::SeqCst);
                        break;
                    }
                    let write_elapsed = write_started.elapsed();
                    let write_ms = write_elapsed.as_millis();
                    max_write_ms = max_write_ms.max(write_ms);
                    sent_packets_total = sent_packets_total.saturating_add(1);

                    let capture_generation_after_write = hub_w.capture_frame_generation();
                    let capture_gen_during_write = capture_generation_after_write
                        .saturating_sub(capture_generation_before_write);
                    if capture_generation_after_write != last_capture_generation {
                        last_capture_generation = capture_generation_after_write;
                        last_capture_progress = Instant::now();
                    }

                    let send_done = Instant::now();
                    if let Some(previous) = last_send_done.replace(send_done) {
                        max_send_gap_ms = max_send_gap_ms.max(send_done.duration_since(previous).as_millis());
                    }

                    if write_elapsed >= RAOP_DIAG_STALL_THRESHOLD {
                        slow_writes_total = slow_writes_total.saturating_add(1);
                        let queue_ms = pending_before_frames.saturating_mul(1000) / sample_rate_w;
                        let queue_after_frames = ring_w
                            .try_lock()
                            .map(|ring| ring.pending_bytes() / bytes_per_frame_w)
                            .ok();
                        if let Ok(mut events) = events_w.lock() {
                            events.push(format!(
                                "MSA RAOP DIAG WRITE-STALL write={}ms queue_before={}f/{}ms queue_after_frames={:?} capture_gen_during_write={} capture_idle={}ms capture_gen={} slow_total={} head_ahead_ms={:?}.",
                                write_ms,
                                pending_before_frames,
                                queue_ms,
                                queue_after_frames,
                                capture_gen_during_write,
                                last_capture_progress.elapsed().as_millis(),
                                last_capture_generation,
                                slow_writes_total,
                                diagnostic_head_ahead_ms(&session_w),
                            ));
                        }
                    }

                    if last_summary.elapsed() >= RAOP_DIAG_SUMMARY_INTERVAL {
                        let queue_ms = pending_before_frames.saturating_mul(1000) / sample_rate_w;
                        let min_frames = if min_queue_frames == usize::MAX { 0 } else { min_queue_frames };
                        let min_ms = min_frames.saturating_mul(1000) / sample_rate_w;
                        let max_ms = max_queue_frames.saturating_mul(1000) / sample_rate_w;
                        if let Ok(mut events) = events_w.lock() {
                            events.push(format!(
                                "MSA RAOP DIAG 10s sent_total={} queue_now={}f/{}ms queue_min={}f/{}ms queue_max={}f/{}ms starvation_total={} max_empty={}ms slow_write_total={} max_write={}ms max_send_gap={}ms capture_idle={}ms capture_gen={} source_present={} reservoir_primed={} reservoir_target={}f/{}ms head_ahead_ms={:?}.",
                                sent_packets_total,
                                pending_before_frames,
                                queue_ms,
                                min_frames,
                                min_ms,
                                max_queue_frames,
                                max_ms,
                                starvation_events_total,
                                max_starvation_ms,
                                slow_writes_total,
                                max_write_ms,
                                max_send_gap_ms,
                                last_capture_progress.elapsed().as_millis(),
                                last_capture_generation,
                                hub_w.source_present(),
                                reservoir_primed,
                                RAOP_RESERVOIR_FRAMES,
                                reservoir_ms_w,
                                diagnostic_head_ahead_ms(&session_w),
                            ));
                        }
                        last_summary = Instant::now();
                        min_queue_frames = usize::MAX;
                        max_queue_frames = 0;
                        max_starvation_ms = 0;
                        max_write_ms = 0;
                        max_send_gap_ms = 0;
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                pcm_source.stop();
                return Err(WindowsRaopWorkerError::Worker(format!(
                    "spawn RAOP writer: {e}"
                )));
            }
        };

        let running_h = Arc::clone(&running);
        let session_h = Arc::clone(&session);
        let error_h = Arc::clone(&last_error);
        let health_worker = match thread::Builder::new()
            .name("msa-raop-health".into())
            .spawn(move || {
                while running_h.load(Ordering::SeqCst) {
                    let alive = session_h
                        .lock()
                        .map(|mut s| s.helper_alive())
                        .unwrap_or(false);
                    if !alive {
                        if let Ok(mut slot) = error_h.lock() {
                            *slot = Some("RAOP transport exited (control/media unhealthy)".into());
                        }
                        running_h.store(false, Ordering::SeqCst);
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }) {
            Ok(worker) => worker,
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                pcm_source.stop();
                let _ = writer_worker.join();
                return Err(WindowsRaopWorkerError::Worker(format!(
                    "spawn RAOP health monitor: {e}"
                )));
            }
        };

        match pcm_source.wait_ready(Duration::from_secs(3)) {
            Ok(()) => {
                if let Ok(mut events) = startup_events.lock() {
                    events.push(
                        "MSA INPUT RAOP first-class lane: shared WindowsPcmSource + WindowsPcmHub active; transport remains pinned libraop."
                            .into(),
                    );
                    events.push(format!(
                        "MSA RAOP RESERVOIR active: AP1-only prime target={}f/{}ms ({} packets); native AP2 untouched.",
                        RAOP_RESERVOIR_FRAMES,
                        raop_reservoir_ms(audio_format.sample_rate.max(1) as usize),
                        RAOP_RESERVOIR_PACKETS,
                    ));
                    events.push(
                        "MSA RAOP DIAG active: queue/starvation-recovery/capture-progress/write-gap/head-ahead telemetry retained."
                            .into(),
                    );
                }
                Ok(Self {
                    session,
                    running,
                    delivery_enabled,
                    send_gate,
                    pcm_hub,
                    pcm_source,
                    first_start_done,
                    writer_worker: Some(writer_worker),
                    health_worker: Some(health_worker),
                    last_error,
                    startup_events,
                })
            }
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                pcm_source.stop();
                let _ = writer_worker.join();
                let _ = health_worker.join();
                Err(WindowsRaopWorkerError::Worker(e.to_string()))
            }
        }
    }

    pub fn session(&self) -> SharedMsaRaopSession { Arc::clone(&self.session) }
    pub fn audio_ready(&self) -> bool { self.pcm_hub.audio_ready() }
    pub fn is_running(&self) -> bool { self.running.load(Ordering::SeqCst) }
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|v| v.clone())
    }
    pub fn discontinuities(&self) -> u64 { self.pcm_source.discontinuities() }
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

    pub fn commit_start(
        &self,
        requested_unix_ms: u64,
    ) -> Result<crate::timing::StartResolution, WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        let ready = session.ready();
        let (live_floor_unix_ms, receiver_latency_ms) = live_source_start_floor_unix_ms(
            unix_now_ms(),
            ready.latency_frames,
            ready.sample_rate,
        );
        let effective_requested_unix_ms = requested_unix_ms.max(live_floor_unix_ms);
        let first = !self.first_start_done.load(Ordering::SeqCst);
        let mut start = if first {
            session.commit_start(effective_requested_unix_ms)?
        } else {
            session.start_after_flush(effective_requested_unix_ms)?
        };
        if first {
            // Same gate as cliairplay session_commit: metadata must land after
            // START commit but before captured PCM delivery opens.
            session.ensure_initial_metadata()?;
        }
        self.first_start_done.store(true, Ordering::SeqCst);
        self.delivery_enabled.store(true, Ordering::SeqCst);

        // Preserve the caller's requested anchor in the public resolution so a
        // coordinator/GUI can see that the Windows live-source floor corrected
        // it.  The accepted instant still comes directly from pinned libraop.
        let accepted_unix_ms = start.at_unix_ms;
        start.requested_unix_ms = requested_unix_ms;
        start.corrected_forward = requested_unix_ms != 0 && accepted_unix_ms != requested_unix_ms;
        if effective_requested_unix_ms != requested_unix_ms {
            if let Ok(mut events) = self.startup_events.lock() {
                events.push(format!(
                    "MSA RAOP LIVE START floor: requested={} effective={} accepted={} receiver_latency={}ms guard={}ms; preserving negotiated receiver headroom for realtime WASAPI.",
                    requested_unix_ms,
                    effective_requested_unix_ms,
                    accepted_unix_ms,
                    receiver_latency_ms,
                    RAOP_LIVE_SOURCE_GUARD_MS,
                ));
            }
        }
        Ok(start)
    }

    pub fn flush_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;

        // Exact MSA boundary: quiesce sender first, FLUSH the receiver, then
        // clear/drain pre-boundary local PCM while sends remain quiesced.
        self.delivery_enabled.store(false, Ordering::SeqCst);
        {
            let mut session = self
                .session
                .lock()
                .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            session.flush()?;
        }

        let generation = self.pcm_hub.request_flush();
        let deadline = Instant::now() + FLUSH_ACK_TIMEOUT;
        while self.pcm_hub.flush_ack_generation() < generation {
            if !self.is_running() {
                return Err(WindowsRaopWorkerError::Worker(
                    "RAOP worker stopped during FLUSH".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(WindowsRaopWorkerError::Worker(
                    "RAOP FLUSH PCM barrier timed out".into(),
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    pub fn standby_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        self.delivery_enabled.store(false, Ordering::SeqCst);
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.standby()?;
        Ok(())
    }

    pub fn pause_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        self.delivery_enabled.store(false, Ordering::SeqCst);
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.pause()?;
        // No local flush-generation bump: new captured content remains in the
        // bounded persistent hub for ACTION=PLAY, matching pinned MSA pause.
        Ok(())
    }

    pub fn play_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.play()?;
        self.delivery_enabled.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn stop_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        self.delivery_enabled.store(false, Ordering::SeqCst);
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.stop()?;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.delivery_enabled.store(false, Ordering::SeqCst);
        self.pcm_source.stop();
        if let Some(w) = self.writer_worker.take() { let _ = w.join(); }
        if let Some(w) = self.health_worker.take() { let _ = w.join(); }
        if let Ok(mut session) = self.session.lock() { session.disconnect(); }
    }
}

impl Drop for WindowsRaopAudioWorker {
    fn drop(&mut self) { self.stop(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_source_floor_keeps_receiver_latency_plus_raop_guard() {
        let (floor, latency_ms) = live_source_start_floor_unix_ms(1_000_000, 99_225, 44_100);
        assert_eq!(latency_ms, 2_250);
        assert_eq!(floor, 1_002_450);
    }

    #[test]
    fn live_source_floor_is_generic_for_other_receiver_latencies() {
        let (floor, latency_ms) = live_source_start_floor_unix_ms(5_000, 44_100, 44_100);
        assert_eq!(latency_ms, 1_000);
        assert_eq!(floor, 6_200);
    }

    #[test]
    fn ap1_reservoir_is_about_96ms_at_44100() {
        assert_eq!(RAOP_RESERVOIR_FRAMES, 4_224);
        assert_eq!(raop_reservoir_ms(44_100), 96);
    }

    #[test]
    fn ap1_reservoir_remains_packet_based_at_48000() {
        assert_eq!(raop_reservoir_ms(48_000), 88);
    }
}
