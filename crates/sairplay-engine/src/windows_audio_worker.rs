use crate::{
    system_time_to_ntp, Pcm352Chunker, RealtimeMediaSender, WasapiLoopbackCapture,
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

        let worker = thread::spawn(move || {
            let capture = match WasapiLoopbackCapture::open_default() {
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

            let mut chunker = Pcm352Chunker::new();
            let mut captured_frames_total = 0u64;
            let mut source_present = false;
            let mut cold_armed = false;
            let mut startup_packet_index: u32 = 0;
            let mut startup_started: Option<std::time::Instant> = None;
            let mut input_starved_since: Option<std::time::Instant> = None;
            let mut nonzero_gap_started: Option<std::time::Instant> = None;
            let mut nonzero_gap_reported = false;
            let mut resume_packet_pending = false;
            let mut inferred_idle = false;
            let mut idle_keepalive_reported = false;

            while running_thread.load(Ordering::SeqCst) {
                match capture.drain_into(&mut chunker) {
                    Ok(report) => {
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

                        // Transition diagnostics only: distinguish a real PCM
                        // return from ordinary WASAPI packet cadence. This does
                        // not alter queued PCM or sender behavior.
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
                                    }
                                }
                                if inferred_idle {
                                    inferred_idle = false;
                                    idle_keepalive_reported = false;
                                    input_starved_since = None;
                                }
                                nonzero_gap_reported = false;
                            } else {
                                let gap_started = nonzero_gap_started
                                    .get_or_insert_with(std::time::Instant::now);
                                if !nonzero_gap_reported
                                    && gap_started.elapsed() >= Duration::from_millis(250)
                                {
                                    if let Ok(mut events) = startup_events_thread.lock() {
                                        events.push(format!(
                                            "Transition: no nonzero PCM for >=250 ms · wasapi_frames={} · engine_non_silent={} · pending_bytes={} · pad_debt={} · reanchors={}.",
                                            frames,
                                            report.first_non_silent_frame_offset.is_some(),
                                            chunker.pending_bytes(),
                                            sender.splice_pad_frames(),
                                            sender.timeline_reanchors()
                                        ));
                                    }
                                    inferred_idle = true;
                                    idle_keepalive_reported = false;
                                    input_starved_since = None;
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

                        // A sustained all-zero Windows loopback interval is
                        // the local equivalent of source PAUSED/EOF/track-gap:
                        // keep the splice line hot and do not re-anchor it.
                        // This is intentionally gated on actual PCM content
                        // (first_nonzero), not on a single empty WASAPI poll.
                        if inferred_idle {
                            input_starved_since = None;
                        } else if chunker.has_packet() {
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

                        // Upstream keeps an already-started splice timeline
                        // bitstream-continuous with encoded silence while
                        // PAUSED/EOF/idle. Windows loopback has no command pipe,
                        // so inferred_idle supplies only that missing state.
                        if inferred_idle && !chunker.has_packet() {
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
                            if sender.can_accept_frames(ntp) {
                                let silence = [0u8; crate::PCM352_PACKET_BYTES];
                                match sender.send_pcm_352(&silence, ntp, lead_frames) {
                                    Ok(result) => {
                                        if !idle_keepalive_reported {
                                            if let Ok(mut events) = startup_events_thread.lock() {
                                                events.push(format!(
                                                    "Transition: inferred idle keepalive active · seq={} ts={} audio_sent={} · pending_bytes={} · pad_debt={}.",
                                                    result.sequence_sent,
                                                    result.timestamp_sent,
                                                    result.audio_delivered,
                                                    chunker.pending_bytes(),
                                                    sender.splice_pad_frames()
                                                ));
                                            }
                                            idle_keepalive_reported = true;
                                        }
                                    }
                                    Err(error) => {
                                        if let Ok(mut slot) = last_error_thread.lock() {
                                            *slot = Some(format!("idle silence keepalive failed: {error:?}"));
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                        }

                        loop {
                            let pad_now = sender.splice_pad_frames().min(352);
                            let real_frames_needed = 352usize - pad_now as usize;
                            let real_bytes_needed = real_frames_needed * 4;

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

                            let packet = chunker
                                .pop_packet_with_silence_prefix(pad_now)
                                .expect("required real-byte count checked");
                            match sender.send_pcm_352(&packet, ntp, lead_frames) {
                                Ok(result) => {
                                    startup_packet_index = startup_packet_index.saturating_add(1);
                                    if resume_packet_pending {
                                        if let Ok(mut events) = startup_events_thread.lock() {
                                            events.push(format!(
                                                "Transition: first outbound after PCM resume · seq={} ts={} marker={} sync_sent={} audio_sent={} pad_before={} · pending_after={}.",
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

impl Drop for WindowsAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
