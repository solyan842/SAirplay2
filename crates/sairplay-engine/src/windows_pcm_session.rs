use crate::{Ap2AudioFormat, WasapiLoopbackCapture, WasapiLoopbackError};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const WINDOWS_PCM_SESSION_RING_SECONDS: usize = 4;
pub const WINDOWS_PCM_SESSION_RING_MIN_BYTES: usize = 1 << 20;
const PRODUCER_WAIT_SLICE: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsPcmDiscontinuity {
    pub count: u64,
    pub cumulative: u64,
    pub absolute_frame: Option<u64>,
}

#[derive(Debug)]
struct RingState {
    data: VecDeque<u8>,
    stopped: bool,
    error: Option<String>,
}

#[derive(Debug)]
struct SharedRing {
    state: Mutex<RingState>,
    can_read: Condvar,
    can_write: Condvar,
    running: AtomicBool,
}

pub struct WindowsPcmSession {
    shared: Arc<SharedRing>,
    producer: Option<JoinHandle<()>>,
    packet_bytes: usize,
    capacity_bytes: usize,
    byte_rate: usize,
    captured_frames: Arc<AtomicU64>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
    discontinuity_events: Arc<Mutex<Vec<WindowsPcmDiscontinuity>>>,
}

impl WindowsPcmSession {
    /// Windows adaptation of Music Assistant's persistent ap2_session input reader.
    ///
    /// The WASAPI COM capture client lives only on the producer thread. That thread
    /// continuously drains Windows audio into one bounded session ring; the
    /// transport worker consumes from the ring independently. This keeps capture
    /// draining even while START planning, PTP readiness, RTSP, ALAC or network
    /// sending blocks the transport side.
    pub fn start(audio_format: Ap2AudioFormat) -> Result<Self, WasapiLoopbackError> {
        let bytes_per_frame = audio_format.input_bytes_per_frame();
        let packet_bytes = 352usize.saturating_mul(bytes_per_frame);
        let byte_rate = (audio_format.sample_rate as usize).saturating_mul(bytes_per_frame);
        let capacity_bytes = byte_rate
            .saturating_mul(WINDOWS_PCM_SESSION_RING_SECONDS)
            .max(WINDOWS_PCM_SESSION_RING_MIN_BYTES);

        let shared = Arc::new(SharedRing {
            state: Mutex::new(RingState {
                data: VecDeque::with_capacity(capacity_bytes),
                stopped: false,
                error: None,
            }),
            can_read: Condvar::new(),
            can_write: Condvar::new(),
            running: AtomicBool::new(true),
        });

        let captured_frames = Arc::new(AtomicU64::new(0));
        let discontinuities = Arc::new(AtomicU64::new(0));
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let discontinuity_events = Arc::new(Mutex::new(Vec::new()));

        let shared_thread = Arc::clone(&shared);
        let captured_frames_thread = Arc::clone(&captured_frames);
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_frame_thread = Arc::clone(&last_discontinuity_frame);
        let discontinuity_events_thread = Arc::clone(&discontinuity_events);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let producer = thread::Builder::new()
            .name("sairplay-wasapi-reader".into())
            .spawn(move || {
                let capture = match WasapiLoopbackCapture::open_default_for_format(audio_format) {
                    Ok(capture) => {
                        let _ = ready_tx.send(Ok(()));
                        capture
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        finish_with_error(&shared_thread, message);
                        return;
                    }
                };

                let mut raw = Vec::<u8>::with_capacity(byte_rate / 10 + packet_bytes);

                while shared_thread.running.load(Ordering::SeqCst) {
                    let report = match capture.drain_into_bytes(&mut raw) {
                        Ok(report) => report,
                        Err(error) => {
                            finish_with_error(&shared_thread, error.to_string());
                            return;
                        }
                    };

                    let captured_before = captured_frames_thread.load(Ordering::SeqCst);
                    if report.discontinuities != 0 {
                        let cumulative = discontinuities_thread
                            .fetch_add(report.discontinuities, Ordering::SeqCst)
                            .saturating_add(report.discontinuities);
                        let absolute_frame = report
                            .discontinuity_frame_offset
                            .map(|offset| captured_before.saturating_add(offset));
                        if let Some(frame) = absolute_frame {
                            last_discontinuity_frame_thread.store(frame, Ordering::SeqCst);
                        }
                        if let Ok(mut events) = discontinuity_events_thread.lock() {
                            events.push(WindowsPcmDiscontinuity {
                                count: report.discontinuities,
                                cumulative,
                                absolute_frame,
                            });
                        }
                    }
                    captured_frames_thread.fetch_add(report.frames as u64, Ordering::SeqCst);

                    if raw.is_empty() {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }

                    let mut offset = 0usize;
                    while offset < raw.len() && shared_thread.running.load(Ordering::SeqCst) {
                        let mut state = match shared_thread.state.lock() {
                            Ok(state) => state,
                            Err(_) => {
                                finish_with_error(
                                    &shared_thread,
                                    "Windows PCM session ring mutex poisoned".into(),
                                );
                                return;
                            }
                        };

                        while state.data.len() == capacity_bytes
                            && shared_thread.running.load(Ordering::SeqCst)
                        {
                            let waited = shared_thread
                                .can_write
                                .wait_timeout(state, PRODUCER_WAIT_SLICE);
                            match waited {
                                Ok((next, _)) => state = next,
                                Err(_) => {
                                    finish_with_error(
                                        &shared_thread,
                                        "Windows PCM session ring wait poisoned".into(),
                                    );
                                    return;
                                }
                            }
                        }

                        if !shared_thread.running.load(Ordering::SeqCst) {
                            break;
                        }

                        let space = capacity_bytes.saturating_sub(state.data.len());
                        let take = space.min(raw.len().saturating_sub(offset));
                        if take == 0 {
                            continue;
                        }
                        state
                            .data
                            .extend(raw[offset..offset.saturating_add(take)].iter().copied());
                        offset = offset.saturating_add(take);
                        shared_thread.can_read.notify_all();
                    }
                }

                mark_stopped(&shared_thread);
            })
            .map_err(|e| WasapiLoopbackError::Windows(format!(
                "cannot start persistent WASAPI reader: {e}"
            )))?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                shared,
                producer: Some(producer),
                packet_bytes,
                capacity_bytes,
                byte_rate,
                captured_frames,
                discontinuities,
                last_discontinuity_frame,
                discontinuity_events,
            }),
            Ok(Err(message)) => {
                shared.running.store(false, Ordering::SeqCst);
                shared.can_read.notify_all();
                shared.can_write.notify_all();
                let _ = producer.join();
                Err(WasapiLoopbackError::Windows(message))
            }
            Err(error) => {
                shared.running.store(false, Ordering::SeqCst);
                shared.can_read.notify_all();
                shared.can_write.notify_all();
                let _ = producer.join();
                Err(WasapiLoopbackError::Windows(format!(
                    "persistent WASAPI reader startup timeout: {error}"
                )))
            }
        }
    }

    pub fn packet_bytes(&self) -> usize {
        self.packet_bytes
    }

    pub fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }

    pub fn byte_rate(&self) -> usize {
        self.byte_rate
    }

    pub fn buffered_bytes(&self) -> usize {
        self.shared
            .state
            .lock()
            .map(|state| state.data.len())
            .unwrap_or(0)
    }

    pub fn buffered_ms(&self) -> u64 {
        if self.byte_rate == 0 {
            return 0;
        }
        ((self.buffered_bytes() as u128 * 1000) / self.byte_rate as u128) as u64
    }

    /// Wait for at least `want` bytes without consuming them.
    pub fn wait_ready(&self, want: usize, timeout: Duration) -> Result<bool, String> {
        if want == 0 {
            return Ok(true);
        }

        let deadline = Instant::now() + timeout;
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| "Windows PCM session ring mutex poisoned".to_string())?;

        loop {
            if let Some(error) = state.error.clone() {
                return Err(error);
            }
            if state.data.len() >= want {
                return Ok(true);
            }
            if state.stopped || !self.shared.running.load(Ordering::SeqCst) {
                return Err("Windows PCM session reader stopped".into());
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, wait) = self
                .shared
                .can_read
                .wait_timeout(state, remaining)
                .map_err(|_| "Windows PCM session ring wait poisoned".to_string())?;
            state = next;
            if wait.timed_out() && state.data.len() < want {
                return Ok(false);
            }
        }
    }

    /// Read exactly `want` bytes when available. A timeout means source
    /// starvation for this interval; it is never treated as EOF.
    pub fn read_exact_timeout(
        &self,
        want: usize,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        if want == 0 {
            return Ok(Some(Vec::new()));
        }

        let deadline = Instant::now() + timeout;
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| "Windows PCM session ring mutex poisoned".to_string())?;

        loop {
            if let Some(error) = state.error.clone() {
                return Err(error);
            }

            if state.data.len() >= want {
                let mut out = Vec::with_capacity(want);
                for _ in 0..want {
                    out.push(
                        state
                            .data
                            .pop_front()
                            .expect("ring length checked before pop"),
                    );
                }
                self.shared.can_write.notify_all();
                return Ok(Some(out));
            }

            if state.stopped || !self.shared.running.load(Ordering::SeqCst) {
                return Err("Windows PCM session reader stopped".into());
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, wait) = self
                .shared
                .can_read
                .wait_timeout(state, remaining)
                .map_err(|_| "Windows PCM session ring wait poisoned".to_string())?;
            state = next;
            if wait.timed_out() && state.data.len() < want {
                return Ok(None);
            }
        }
    }

    pub fn captured_frames(&self) -> u64 {
        self.captured_frames.load(Ordering::SeqCst)
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

    pub fn drain_discontinuity_events(&self) -> Vec<WindowsPcmDiscontinuity> {
        match self.discontinuity_events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(_) => Vec::new(),
        }
    }

    pub fn stop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        mark_stopped(&self.shared);
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

impl Drop for WindowsPcmSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mark_stopped(shared: &SharedRing) {
    if let Ok(mut state) = shared.state.lock() {
        state.stopped = true;
    }
    shared.can_read.notify_all();
    shared.can_write.notify_all();
}

fn finish_with_error(shared: &SharedRing, message: String) {
    shared.running.store(false, Ordering::SeqCst);
    if let Ok(mut state) = shared.state.lock() {
        state.error = Some(message);
        state.stopped = true;
    }
    shared.can_read.notify_all();
    shared.can_write.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_capacity_matches_msa_four_second_or_one_mib_floor() {
        let fmt16 = Ap2AudioFormat::ALAC_44100_16_STEREO;
        let byte_rate16 =
            fmt16.sample_rate as usize * fmt16.input_bytes_per_frame();
        assert_eq!(
            byte_rate16
                .saturating_mul(WINDOWS_PCM_SESSION_RING_SECONDS)
                .max(WINDOWS_PCM_SESSION_RING_MIN_BYTES),
            WINDOWS_PCM_SESSION_RING_MIN_BYTES
        );

        let fmt24 = Ap2AudioFormat::ALAC_48000_24_STEREO;
        let byte_rate24 =
            fmt24.sample_rate as usize * fmt24.input_bytes_per_frame();
        assert!(
            byte_rate24
                .saturating_mul(WINDOWS_PCM_SESSION_RING_SECONDS)
                >= WINDOWS_PCM_SESSION_RING_MIN_BYTES
        );
    }

    #[test]
    fn ready_threshold_is_one_complete_transport_packet() {
        for format in [
            Ap2AudioFormat::ALAC_44100_16_STEREO,
            Ap2AudioFormat::ALAC_48000_16_STEREO,
            Ap2AudioFormat::ALAC_44100_24_STEREO,
            Ap2AudioFormat::ALAC_48000_24_STEREO,
        ] {
            assert_eq!(
                352 * format.input_bytes_per_frame(),
                if format.bit_depth > 16 { 352 * 8 } else { 352 * 4 }
            );
        }
    }
}
