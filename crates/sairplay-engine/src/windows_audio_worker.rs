use crate::{
    system_time_to_ntp, Pcm352Chunker, RealtimeMediaSender, WasapiLoopbackCapture,
    WasapiLoopbackError,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
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
                    Ok(frames) => {
                        while chunker.has_packet() {
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

                            // Match upstream ap2cl_accept_frames(): keep the PCM
                            // queued until the source timeline's pacing gate opens.
                            if !sender.can_accept_frames(ntp) {
                                thread::sleep(Duration::from_millis(1));
                                break;
                            }

                            let packet = chunker.pop_packet().expect("has_packet checked");
                            if let Err(error) = sender.send_pcm_352(&packet, ntp, lead_frames) {
                                if let Ok(mut slot) = last_error_thread.lock() {
                                    *slot = Some(format!("media send failed: {error:?}"));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        }

                        if frames == 0 && !chunker.has_packet() {
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

                            // Source splice/starvation contract: temporary PCM
                            // absence never stops the wire. Consume any partial
                            // tail once, pad the rest with encoded silence, and
                            // keep advancing the same RTP/anchor timeline.
                            if sender.can_accept_frames(ntp) {
                                let packet = chunker.pop_packet_padded_silence();
                                if let Err(error) = sender.send_pcm_352(&packet, ntp, lead_frames) {
                                    if let Ok(mut slot) = last_error_thread.lock() {
                                        *slot = Some(format!("silence keepalive send failed: {error:?}"));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            } else {
                                thread::sleep(Duration::from_millis(1));
                            }
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
