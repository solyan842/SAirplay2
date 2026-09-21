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
    ) -> Result<Self, WindowsAudioWorkerError> {
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
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
                match capture.drain_into(&mut chunker) {
                    Ok(report) => {
                        if report.discontinuities != 0 {
                            discontinuities_thread.fetch_add(report.discontinuities, Ordering::SeqCst);
                        }
                        let frames = report.frames;
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

                        // Source delivery-stall guard runs while content is
                        // still queued. It only pads once the wire head is
                        // actually behind wall clock.
                        if chunker.has_packet() {
                            let _ = sender.recover_delivery_gap(recovery_ntp, lead_frames);
                        } else if frames == 0 {
                            // Source starvation guard is anticipatory: when
                            // input is dry it starts padding as the effective
                            // head enters the 250 ms minimum-lead floor.
                            let _ = sender.recover_input_gap(recovery_ntp, lead_frames);
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
                            if let Err(error) = sender.send_pcm_352(&packet, ntp, lead_frames) {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!("media send failed: {error:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
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

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WindowsAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
