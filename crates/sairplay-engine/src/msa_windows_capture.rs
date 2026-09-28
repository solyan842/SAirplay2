//! Producer-only WASAPI capture worker for the SAirplay 2.0 MSA clone.
//!
//! Unlike the legacy WindowsAudioWorker this worker owns no sender, no START
//! timing and no inferred playback state. It only drains Windows loopback PCM
//! into the persistent MSA source ring. When that ring is full it stops draining
//! until space becomes available, mirroring MSA's reader waiting on can_write.

use crate::{
    MsaWindowsSource, Pcm352Chunker, WasapiLoopbackCapture, WasapiLoopbackError,
    WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug)]
pub enum MsaWindowsCaptureError {
    Capture(WasapiLoopbackError),
    SourcePoisoned,
}

impl fmt::Display for MsaWindowsCaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => write!(f, "{error}"),
            Self::SourcePoisoned => write!(f, "MSA Windows source lock poisoned"),
        }
    }
}

impl std::error::Error for MsaWindowsCaptureError {}

pub struct MsaWindowsCaptureWorker {
    source: Arc<Mutex<MsaWindowsSource>>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
}

impl MsaWindowsCaptureWorker {
    pub fn start(source: Arc<Mutex<MsaWindowsSource>>) -> Result<Self, MsaWindowsCaptureError> {
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let source_thread = Arc::clone(&source);
        let last_error = Arc::new(Mutex::new(None));
        let last_error_thread = Arc::clone(&last_error);
        let discontinuities = Arc::new(AtomicU64::new(0));
        let discontinuities_thread = Arc::clone(&discontinuities);
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

            while running_thread.load(Ordering::SeqCst) {
                // First drain any already-staged complete packets into the
                // bounded MSA ring. Do not pop until the ring can accept the
                // whole packet: this preserves packet bytes under backpressure.
                loop {
                    if !chunker.has_packet() {
                        break;
                    }
                    let room = match source_thread.lock() {
                        Ok(source) => source.input_room_bytes(),
                        Err(_) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some("MSA Windows source lock poisoned".into());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    };
                    if room < WINDOWS_PCM_PACKET_BYTES_16_441_STEREO {
                        break;
                    }

                    let packet = chunker.pop_packet().expect("packet presence checked");
                    match source_thread.lock() {
                        Ok(mut source) => {
                            let pushed = source.push_capture_pcm(&packet);
                            if pushed != packet.len() {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!(
                                        "MSA Windows source accepted only {pushed}/{} bytes",
                                        packet.len()
                                    ));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                        Err(_) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some("MSA Windows source lock poisoned".into());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }

                // Match MSA can_write backpressure: while a complete staged
                // packet cannot enter the ring, do not drain more WASAPI data.
                if chunker.has_packet() {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                match capture.drain_into(&mut chunker) {
                    Ok(report) => {
                        if report.discontinuities != 0 {
                            discontinuities_thread
                                .fetch_add(report.discontinuities, Ordering::SeqCst);
                        }
                        // Zero/SILENT PCM is intentionally left in the chunker.
                        // There is no source_present gate and no amplitude-based
                        // state transition in this worker.
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
                source,
                running,
                worker: Some(worker),
                last_error,
                discontinuities,
            }),
            Ok(Err(message)) => {
                let _ = worker.join();
                Err(MsaWindowsCaptureError::Capture(
                    WasapiLoopbackError::Windows(message),
                ))
            }
            Err(error) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(MsaWindowsCaptureError::Capture(
                    WasapiLoopbackError::Windows(format!(
                        "MSA WASAPI capture startup timeout: {error}"
                    )),
                ))
            }
        }
    }

    pub fn source(&self) -> &Arc<Mutex<MsaWindowsSource>> {
        &self.source
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

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MsaWindowsCaptureWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_room_tracks_session_ring_not_pcm_amplitude() {
        let mut source = MsaWindowsSource::new(
            44_100 * 2 * 2,
            WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
        )
        .unwrap();

        let before = source.input_room_bytes();
        let zero = vec![0u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO];
        assert_eq!(source.push_capture_pcm(&zero), zero.len());
        assert_eq!(source.input_room_bytes(), before - zero.len());
        assert!(source.audio_present());
    }
}
