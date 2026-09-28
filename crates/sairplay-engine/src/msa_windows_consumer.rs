//! Consumer-only Windows media loop for the SAirplay 2.0 MSA clone.
//!
//! This loop deliberately owns no START/FLUSH/STANDBY decisions. The lifecycle
//! adapter arms/reanchors the transport first; only then may MsaSessionCore enter
//! PLAYING, which is the sole condition under which PCM can be consumed here.
//!
//! No amplitude-based state inference and no legacy gap-recovery/reanchor calls
//! are permitted in this path. A temporarily empty ring is starvation only.

use crate::{
    system_time_to_ntp, MediaSendError, MsaWindowsSource, RealtimeMediaSender,
    WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

pub trait MsaWindowsPacketSink: Send + 'static {
    fn can_accept_packet(&mut self, now_ntp: u64) -> bool;
    fn send_packet(&mut self, pcm: &[u8], now_ntp: u64) -> Result<(), String>;
}

pub struct RealtimeMsaPacketSink {
    sender: RealtimeMediaSender,
    lead_frames: u32,
}

impl RealtimeMsaPacketSink {
    pub fn new(sender: RealtimeMediaSender, lead_frames: u32) -> Self {
        Self {
            sender,
            lead_frames,
        }
    }

    pub fn sender(&self) -> &RealtimeMediaSender {
        &self.sender
    }

    pub fn sender_mut(&mut self) -> &mut RealtimeMediaSender {
        &mut self.sender
    }
}

impl MsaWindowsPacketSink for RealtimeMsaPacketSink {
    fn can_accept_packet(&mut self, now_ntp: u64) -> bool {
        self.sender.can_accept_frames(now_ntp)
    }

    fn send_packet(&mut self, pcm: &[u8], now_ntp: u64) -> Result<(), String> {
        self.sender
            .send_pcm_352(pcm, now_ntp, self.lead_frames)
            .map(|_| ())
            .map_err(|error: MediaSendError| format!("{error:?}"))
    }
}

#[derive(Debug)]
pub enum MsaWindowsConsumerError {
    SourcePoisoned,
    Time(String),
    Media(String),
}

impl fmt::Display for MsaWindowsConsumerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourcePoisoned => write!(f, "MSA Windows source lock poisoned"),
            Self::Time(message) => write!(f, "{message}"),
            Self::Media(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for MsaWindowsConsumerError {}

pub struct MsaWindowsConsumerWorker {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    packets_sent: Arc<AtomicU64>,
}

impl MsaWindowsConsumerWorker {
    pub fn start(
        source: Arc<Mutex<MsaWindowsSource>>,
        mut sink: Box<dyn MsaWindowsPacketSink>,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let last_error = Arc::new(Mutex::new(None));
        let last_error_thread = Arc::clone(&last_error);
        let packets_sent = Arc::new(AtomicU64::new(0));
        let packets_sent_thread = Arc::clone(&packets_sent);

        let worker = thread::spawn(move || {
            while running_thread.load(Ordering::SeqCst) {
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

                if !sink.can_accept_packet(now_ntp) {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                let packet = match source.lock() {
                    Ok(mut source) => match source.next_packet(
                        WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
                    ) {
                        Ok(packet) => packet,
                        Err(error) => {
                            if let Ok(mut slot) = last_error_thread.lock() {
                                *slot = Some(error);
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    },
                    Err(_) => {
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some("MSA Windows source lock poisoned".into());
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let Some(packet) = packet else {
                    // IDLE/STANDBY or temporary PLAYING starvation: never infer
                    // a state change and never synthesize a new anchor here.
                    thread::sleep(Duration::from_millis(1));
                    continue;
                };

                if let Err(error) = sink.send_packet(&packet, now_ntp) {
                    if let Ok(mut slot) = last_error_thread.lock() {
                        *slot = Some(format!("MSA media send failed: {error}"));
                    }
                    running_thread.store(false, Ordering::SeqCst);
                    return;
                }

                packets_sent_thread.fetch_add(1, Ordering::SeqCst);
            }
        });

        Self {
            running,
            worker: Some(worker),
            last_error,
            packets_sent,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn packets_sent(&self) -> u64 {
        self.packets_sent.load(Ordering::SeqCst)
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

impl Drop for MsaWindowsConsumerWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MsaSessionState;
    use std::sync::atomic::AtomicUsize;

    struct FakeSink {
        sends: Arc<AtomicUsize>,
    }

    impl MsaWindowsPacketSink for FakeSink {
        fn can_accept_packet(&mut self, _now_ntp: u64) -> bool {
            true
        }

        fn send_packet(&mut self, _pcm: &[u8], _now_ntp: u64) -> Result<(), String> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn consumer_cannot_drain_idle_source() {
        let source = Arc::new(Mutex::new(
            MsaWindowsSource::new(
                44_100 * 2 * 2,
                WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
            ).unwrap(),
        ));
        source.lock().unwrap().push_capture_pcm(
            &vec![1u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
        );

        let sends = Arc::new(AtomicUsize::new(0));
        let mut worker = MsaWindowsConsumerWorker::start(
            Arc::clone(&source),
            Box::new(FakeSink { sends: Arc::clone(&sends) }),
        );
        thread::sleep(Duration::from_millis(20));
        worker.stop();

        assert_eq!(sends.load(Ordering::SeqCst), 0);
        assert_eq!(
            source.lock().unwrap().buffered_bytes(),
            WINDOWS_PCM_PACKET_BYTES_16_441_STEREO
        );
    }

    #[test]
    fn explicit_start_releases_exactly_buffered_packet() {
        let source = Arc::new(Mutex::new(
            MsaWindowsSource::new(
                44_100 * 2 * 2,
                WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
            ).unwrap(),
        ));
        {
            let mut locked = source.lock().unwrap();
            locked.push_capture_pcm(
                &vec![2u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
            );
            locked.core_mut().start_committed().unwrap();
            assert_eq!(locked.state(), MsaSessionState::Playing);
        }

        let sends = Arc::new(AtomicUsize::new(0));
        let mut worker = MsaWindowsConsumerWorker::start(
            Arc::clone(&source),
            Box::new(FakeSink { sends: Arc::clone(&sends) }),
        );

        for _ in 0..100 {
            if sends.load(Ordering::SeqCst) == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        worker.stop();

        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert_eq!(source.lock().unwrap().buffered_bytes(), 0);
    }

    #[test]
    fn playing_starvation_does_not_change_session_state() {
        let source = Arc::new(Mutex::new(
            MsaWindowsSource::new(
                44_100 * 2 * 2,
                WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
            ).unwrap(),
        ));
        source.lock().unwrap().core_mut().start_committed().unwrap();

        let sends = Arc::new(AtomicUsize::new(0));
        let mut worker = MsaWindowsConsumerWorker::start(
            Arc::clone(&source),
            Box::new(FakeSink { sends: Arc::clone(&sends) }),
        );
        thread::sleep(Duration::from_millis(20));
        worker.stop();

        assert_eq!(sends.load(Ordering::SeqCst), 0);
        assert_eq!(source.lock().unwrap().state(), MsaSessionState::Playing);
    }
}
