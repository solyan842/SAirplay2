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
use std::time::{Duration, Instant, SystemTime};

/// Source value used by Music Assistant for a cold multi-room group start.
/// All members connect first, then one shared audible instant is armed.
pub const AIRPLAY_COLD_GROUP_START_LEAD_MS: u64 = 2_500;

pub struct WindowsAudioTarget {
    pub(crate) name: String,
    pub(crate) sender: RealtimeMediaSender,
    pub(crate) lead_frames: u32,
    pub(crate) latency_max: Option<u32>,
    pub(crate) rtp_offset: u32,
    pub(crate) cold_start_delay_ms: u64,
}

#[derive(Debug)]
pub enum WindowsMultiroomAudioError {
    EmptyGroup,
    Capture(WasapiLoopbackError),
    Media(String),
    Time(String),
}

impl fmt::Display for WindowsMultiroomAudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup => write!(f, "multi-room group has no audio targets"),
            Self::Capture(e) => write!(f, "{e}"),
            Self::Media(e) => write!(f, "{e}"),
            Self::Time(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WindowsMultiroomAudioError {}

pub struct WindowsMultiroomAudioWorker {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
    first_non_silent_frame: Arc<AtomicU64>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
}

impl WindowsMultiroomAudioWorker {
    pub fn start(mut targets: Vec<WindowsAudioTarget>) -> Result<Self, WindowsMultiroomAudioError> {
        if targets.is_empty() {
            return Err(WindowsMultiroomAudioError::EmptyGroup);
        }

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
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        let worker = thread::Builder::new()
            .name("sairplay-multiroom-audio".into())
            .spawn(move || {
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
                let mut input_starved_since: Option<Instant> = None;
                let mut packet_index = 0u64;

                while running_thread.load(Ordering::SeqCst) {
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

                    // Before START, shared-mode engine silence is "no source".
                    if !source_present {
                        if report.first_non_silent_frame_offset.is_some() {
                            source_present = true;
                        } else {
                            chunker.clear();
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                    }

                    // Source contract: every group member is connected before a
                    // single shared cold START. The cold group lead is 2500 ms,
                    // further extended if one receiver still needs clock settle.
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
                        let settle_ms = targets
                            .iter()
                            .map(|target| target.cold_start_delay_ms)
                            .max()
                            .unwrap_or(0);
                        let delay_ms = AIRPLAY_COLD_GROUP_START_LEAD_MS.max(settle_ms);
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
                                        "{} cold group START failed: {error:?}",
                                        target.name
                                    ));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                        cold_armed = true;
                        if let Ok(mut events) = startup_events_thread.lock() {
                            events.push(format!(
                                "MultiRoom: {} members ready · shared cold START={} ms · one WASAPI source.",
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

                    // Keep recovery debt identical across members so the same
                    // source frame lands at the same content position everywhere.
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

                    loop {
                        if targets.is_empty() {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some("all MultiRoom members stopped".into());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }

                        align_splice_pad(&mut targets);
                        let pad_now = targets
                            .iter()
                            .map(|target| target.sender.splice_pad_frames())
                            .max()
                            .unwrap_or(0)
                            .min(352);
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

                        // All members share one source. Gate the release with the
                        // tightest receiver buffer window; larger windows are safe
                        // whenever the smallest one can accept a packet.
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
                            match target.sender.send_pcm_352(&packet, ntp, target.lead_frames) {
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
                                    events.push(format!("MultiRoom member removed: {message}"));
                                }
                                targets.remove(index);
                            }
                            active_members_thread.store(targets.len() as u64, Ordering::SeqCst);
                        }

                        if packet_index <= 8 {
                            if let Ok(mut events) = startup_events_thread.lock() {
                                events.push(format!(
                                    "MultiRoom: packet #{} fan-out to {} members · gate_lead_frames={} · pad={}.",
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
            })
            .map_err(|e| {
                WindowsMultiroomAudioError::Capture(WasapiLoopbackError::Windows(format!(
                    "failed to spawn MultiRoom WASAPI worker: {e}"
                )))
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
                last_discontinuity_frame,
                first_non_silent_frame,
                startup_events,
                active_members,
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
