//! Transport-agnostic Windows PCM producer for MSA receiver sessions.
//!
//! This module owns only WASAPI capture and local PCM/capture metrics. It does
//! not know about AirPlay route policy, RTSP, PTP/NTP, ALAC, RTP, START,
//! PAUSE/PLAY, FLUSH or STOP. A caller may provide an optional diagnostic
//! context callback so capture telemetry can be correlated with receiver state
//! without coupling this source to a transport type.

use crate::{Ap2AudioFormat, WasapiLoopbackCapture, WindowsPcmHub};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub type PcmSourceDiagnosticContext = Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[derive(Debug)]
pub enum WindowsPcmSourceError {
    Spawn(String),
    Ready(String),
}

impl fmt::Display for WindowsPcmSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(message) | Self::Ready(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WindowsPcmSourceError {}

pub struct WindowsPcmSource {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    ready_rx: Receiver<Result<(), String>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
}

impl WindowsPcmSource {
    /// Spawn one WASAPI producer. The caller supplies the shared run gate and
    /// local PCM hub so a media consumer can remain completely independent of
    /// capture scheduling and COM/WASAPI ownership.
    pub fn spawn(
        audio_format: Ap2AudioFormat,
        hub: WindowsPcmHub,
        running: Arc<AtomicBool>,
        last_error: Arc<Mutex<Option<String>>>,
        startup_events: Arc<Mutex<Vec<String>>>,
        diagnostic_context: Option<PcmSourceDiagnosticContext>,
    ) -> Result<Self, WindowsPcmSourceError> {
        let discontinuities = Arc::new(AtomicU64::new(0));
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let running_thread = Arc::clone(&running);
        let hub_thread = hub.clone();
        let ring_thread = hub_thread.ring();
        let error_thread = Arc::clone(&last_error);
        let discontinuities_thread = Arc::clone(&discontinuities);
        let last_discontinuity_thread = Arc::clone(&last_discontinuity_frame);
        let events_thread = Arc::clone(&startup_events);

        let worker = thread::Builder::new()
            .name("sairplay-msa-wasapi".into())
            .spawn(move || {
                let mut capture = match WasapiLoopbackCapture::open_default_for_format(audio_format) {
                    Ok(value) => {
                        if let Ok(mut events) = events_thread.lock() {
                            events.push(format!("MSA INPUT {}.", value.format_summary()));
                            events.push(
                                "MSA INPUT capture/media split active: WASAPI producer cannot be blocked by type103 sender."
                                    .into(),
                            );
                        }
                        let _ = ready_tx.send(Ok(()));
                        value
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = ready_tx.send(Err(message.clone()));
                        if let Ok(mut slot) = error_thread.lock() {
                            *slot = Some(message);
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let ring_capacity = hub_thread.ring_capacity();
                let mut local_flush_generation = hub_thread.flush_generation();
                let mut captured_frames_total = 0u64;
                let mut last_capture_drain = Instant::now();

                while running_thread.load(Ordering::SeqCst) {
                    let generation = hub_thread.flush_generation();
                    if generation != local_flush_generation {
                        let cleared = ring_thread
                            .lock()
                            .map(|mut ring| {
                                let bytes = ring.pending_bytes();
                                ring.clear();
                                bytes
                            })
                            .unwrap_or(0);
                        capture.reset_conversion();
                        hub_thread.reset_source_present();
                        hub_thread.set_audio_ready(false);
                        local_flush_generation = generation;
                        hub_thread.acknowledge_flush(generation);
                        if cleared != 0 {
                            if let Ok(mut events) = events_thread.lock() {
                                events.push(format!(
                                    "MSA INPUT flush reset: discarded {} queued PCM bytes.",
                                    cleared
                                ));
                            }
                        }
                    }

                    let capture_gap_ms = last_capture_drain.elapsed().as_millis();
                    let (pending_before_drain, report, pending_after_drain) = {
                        let mut ring = match ring_thread.lock() {
                            Ok(value) => value,
                            Err(_) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some("PCM ring mutex poisoned".into());
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };
                        let before = ring.pending_bytes();
                        let report = match capture.drain_into(&mut ring) {
                            Ok(value) => value,
                            Err(error) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!("WASAPI capture failed: {error}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };
                        let after = ring.pending_bytes();
                        (before, report, after)
                    };
                    last_capture_drain = Instant::now();

                    hub_thread.note_capture_frames(report.frames);

                    if report.first_non_silent_frame_offset.is_some()
                        && hub_thread.note_non_silent_packet()
                    {
                        if let Ok(mut events) = events_thread.lock() {
                            events.push(
                                "MSA INPUT source-present: first non-SILENT WASAPI packet."
                                    .into(),
                            );
                        }
                    }

                    if report.discontinuities != 0 {
                        let total = discontinuities_thread
                            .fetch_add(report.discontinuities, Ordering::SeqCst)
                            .saturating_add(report.discontinuities);
                        let absolute_frame = report.discontinuity_frame_offset.map(|offset| {
                            captured_frames_total.saturating_add(offset)
                        });
                        if let Some(frame) = absolute_frame {
                            last_discontinuity_thread.store(frame, Ordering::SeqCst);
                        }

                        let bytes_per_frame = audio_format.input_bytes_per_frame().max(1);
                        let pending_before_frames = pending_before_drain / bytes_per_frame;
                        let pending_after_frames = pending_after_drain / bytes_per_frame;
                        let pending_excess_bytes = pending_after_drain.saturating_sub(ring_capacity);
                        let context = diagnostic_context
                            .as_ref()
                            .and_then(|snapshot| snapshot());

                        if let Ok(mut events) = events_thread.lock() {
                            match context {
                                Some(context) => events.push(format!(
                                    "MSA INPUT DISCONTINUITY diag total={} batch={} capture_gap={}ms drained_frames={} output_frames={} silent_frames={} pending_before={}f/{}B pending_after={}f/{}B excess={}B discontinuity_frame={:?} {}.",
                                    total,
                                    report.discontinuities,
                                    capture_gap_ms,
                                    report.frames,
                                    report.output_frames,
                                    report.silent_frames,
                                    pending_before_frames,
                                    pending_before_drain,
                                    pending_after_frames,
                                    pending_after_drain,
                                    pending_excess_bytes,
                                    absolute_frame,
                                    context,
                                )),
                                None => events.push(format!(
                                    "MSA INPUT DISCONTINUITY diag total={} batch={} capture_gap={}ms drained_frames={} output_frames={} silent_frames={} pending_before={}f/{}B pending_after={}f/{}B excess={}B discontinuity_frame={:?}; engine diagnostics busy/unavailable.",
                                    total,
                                    report.discontinuities,
                                    capture_gap_ms,
                                    report.frames,
                                    report.output_frames,
                                    report.silent_frames,
                                    pending_before_frames,
                                    pending_before_drain,
                                    pending_after_frames,
                                    pending_after_drain,
                                    pending_excess_bytes,
                                    absolute_frame,
                                )),
                            }
                        }
                    }

                    captured_frames_total =
                        captured_frames_total.saturating_add(report.frames as u64);

                    if !hub_thread.source_present() {
                        if let Ok(mut ring) = ring_thread.lock() {
                            ring.clear();
                        }
                        capture.reset_conversion();
                        hub_thread.set_audio_ready(false);
                    } else {
                        let (dropped, has_packet) = match ring_thread.lock() {
                            Ok(mut ring) => {
                                let dropped = ring.truncate_pending(ring_capacity);
                                (dropped, ring.has_packet())
                            }
                            Err(_) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some("PCM ring mutex poisoned".into());
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        };
                        hub_thread.set_audio_ready(has_packet);
                        if dropped != 0 {
                            if let Ok(mut events) = events_thread.lock() {
                                events.push(format!(
                                    "MSA INPUT bounded ring full: discarded {} newest PCM bytes (capacity={}B).",
                                    dropped, ring_capacity
                                ));
                            }
                        }
                    }

                    if report.frames == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            })
            .map_err(|error| {
                WindowsPcmSourceError::Spawn(format!("spawn WASAPI producer: {error}"))
            })?;

        Ok(Self {
            running,
            worker: Some(worker),
            ready_rx,
            discontinuities,
            last_discontinuity_frame,
        })
    }

    pub fn wait_ready(&self, timeout: Duration) -> Result<(), WindowsPcmSourceError> {
        match self.ready_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(WindowsPcmSourceError::Ready(message)),
            Err(_) => Err(WindowsPcmSourceError::Ready(
                "WASAPI producer did not become ready".into(),
            )),
        }
    }

    pub fn discontinuities(&self) -> u64 {
        self.discontinuities.load(Ordering::SeqCst)
    }

    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        let value = self.last_discontinuity_frame.load(Ordering::SeqCst);
        (value != u64::MAX).then_some(value)
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WindowsPcmSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_context_is_transport_agnostic_text() {
        let context: PcmSourceDiagnosticContext = Arc::new(|| {
            Some("state=Streaming head_frame=7 audio_sent=9".into())
        });
        assert_eq!(
            context(),
            Some("state=Streaming head_frame=7 audio_sent=9".into())
        );
    }

    #[test]
    fn source_error_is_plain_adapter_error() {
        let error = WindowsPcmSourceError::Ready("capture not ready".into());
        assert_eq!(error.to_string(), "capture not ready");
    }
}
