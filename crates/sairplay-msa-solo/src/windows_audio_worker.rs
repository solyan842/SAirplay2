//! Windows WASAPI -> native AP2 SOLO media worker.
//! The worker owns the COM/WASAPI capture apartment; the engine stays behind
//! a shared mutex so lifecycle commands can serialize with media sends.

use crate::{
    ap2::Ap2State,
    native_media::SendResult,
    native_timeline::ntp_to_frames,
    ntp_timing::system_time_to_ntp,
    Ap2AudioFormat, NativeSoloEngine, Pcm352Chunker, WasapiLoopbackCapture,
    WasapiLoopbackError,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub const AIRPLAY_CLOCK_READY_TIMEOUT: Duration = Duration::from_millis(2500);
pub const STARVATION_RECOVERY_INTERVAL: Duration = Duration::from_millis(250);
pub const FLUSH_DRAIN_TIMEOUT: Duration = Duration::from_millis(2000);

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
        let last_error = Arc::new(Mutex::new(None));
        let discontinuities = Arc::new(AtomicU64::new(0));
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));

        let running_thread = Arc::clone(&running);
        let flush_thread = Arc::clone(&flush_generation);
        let flush_ack_thread = Arc::clone(&flush_ack_generation);
        let error_thread = Arc::clone(&last_error);
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_thread = Arc::clone(&last_discontinuity_frame);
        let events_thread = Arc::clone(&startup_events);
        let engine_thread = Arc::clone(&engine);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("sairplay-msa-wasapi".into())
            .spawn(move || {
                let capture = match WasapiLoopbackCapture::open_default_for_format(audio_format) {
                    Ok(v) => {
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
                let mut local_flush_generation = flush_thread.load(Ordering::SeqCst);
                let mut cold_started = false;
                let mut ptp_wait_started: Option<Instant> = None;
                let mut starvation_started: Option<Instant> = None;
                let mut last_starvation_recovery: Option<Instant> = None;

                while running_thread.load(Ordering::SeqCst) {
                    let generation = flush_thread.load(Ordering::SeqCst);
                    if generation != local_flush_generation {
                        chunker.clear();
                        local_flush_generation = generation;
                        starvation_started = None;
                        last_starvation_recovery = None;
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
                    // MSA's persistent input ring is max(4 seconds, 1 MiB).
                    // WASAPI cannot backpressure the system mixer, so preserve
                    // the oldest resident bytes and discard only new excess.
                    let byte_rate = audio_format.sample_rate as usize
                        * audio_format.input_bytes_per_frame();
                    let ring_capacity = (byte_rate.saturating_mul(4)).max(1 << 20);
                    let _ = chunker.truncate_pending(ring_capacity);

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

                    if !cold_started {
                        if !chunker.has_packet() {
                            if report.frames == 0 {
                                thread::sleep(Duration::from_millis(1));
                            }
                            continue;
                        }

                        let should_wait_for_ptp = {
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
                            guard.ready.timing_owner.use_ptp()
                                && guard.ready.timing_owner.probe_streak().is_none()
                        };
                        if should_wait_for_ptp {
                            let since = ptp_wait_started.get_or_insert_with(Instant::now);
                            if since.elapsed() < AIRPLAY_CLOCK_READY_TIMEOUT {
                                if report.frames == 0 {
                                    thread::sleep(Duration::from_millis(1));
                                }
                                continue;
                            }
                        } else {
                            ptp_wait_started = None;
                        }

                        let start = {
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
                            guard.start(0)
                        };
                        match start {
                            Ok(resolution) => {
                                cold_started = true;
                                if let Ok(mut events) = events_thread.lock() {
                                    events.push(format!(
                                        "WASAPI cold START committed at {} ms · format={}/{} · pending_bytes={}.",
                                        resolution.at_unix_ms,
                                        audio_format.bit_depth,
                                        audio_format.sample_rate,
                                        chunker.pending_bytes(),
                                    ));
                                }
                            }
                            Err(e) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!("native SOLO cold START failed: {e:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
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
                            let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                                Ok(v) => v,
                                Err(e) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some(format!("clock conversion failed: {e:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            };
                            let now_frame =
                                ntp_to_frames(now_ntp, guard.runtime.media.timeline.sample_rate);
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
                                    let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                                        Ok(v) => v,
                                        Err(_) => 0,
                                    };
                                    let now_frame = ntp_to_frames(
                                        now_ntp,
                                        guard.runtime.media.timeline.sample_rate,
                                    );
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
    pub fn commit_start(
        &self,
        requested_unix_ms: u64,
    ) -> Result<crate::timing::StartResolution, WindowsSoloAudioWorkerError> {
        let mut engine = self.engine.lock().map_err(|_| {
            WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
        })?;
        engine.start(requested_unix_ms).map_err(|e| {
            WindowsSoloAudioWorkerError::Engine(format!("START: {e:?}"))
        })
    }

    pub fn standby_content(&self) -> Result<(), WindowsSoloAudioWorkerError> {
        let mut engine = self.engine.lock().map_err(|_| {
            WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
        })?;
        engine.standby().map_err(|e| {
            WindowsSoloAudioWorkerError::Engine(format!("STANDBY: {e:?}"))
        })
    }

    pub fn set_content_enabled(&self, enabled: bool) -> Result<(), WindowsSoloAudioWorkerError> {
        let mut engine = self.engine.lock().map_err(|_| {
            WindowsSoloAudioWorkerError::Engine("native SOLO engine mutex poisoned".into())
        })?;
        if enabled {
            engine.play_content().map_err(|e| WindowsSoloAudioWorkerError::Engine(format!("play: {e:?}")))
        } else {
            engine.pause_content().map_err(|e| WindowsSoloAudioWorkerError::Engine(format!("pause: {e:?}")))
        }
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

    pub fn stop(&mut self) {
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
