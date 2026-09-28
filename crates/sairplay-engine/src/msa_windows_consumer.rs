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
use std::time::{Duration, Instant, SystemTime};

pub trait MsaWindowsPacketSink: Send + 'static {
    fn can_accept_packet(&mut self, now_ntp: u64) -> bool;
    fn send_packet(&mut self, pcm: &[u8], now_ntp: u64) -> Result<(), String>;
}

pub type SharedRealtimeMediaSender = Arc<Mutex<RealtimeMediaSender>>;

pub struct RealtimeMsaPacketSink {
    sender: SharedRealtimeMediaSender,
    lead_frames: u32,
}

impl RealtimeMsaPacketSink {
    pub fn new(sender: RealtimeMediaSender, lead_frames: u32) -> Self {
        Self::from_shared(Arc::new(Mutex::new(sender)), lead_frames)
    }

    pub fn from_shared(sender: SharedRealtimeMediaSender, lead_frames: u32) -> Self {
        Self {
            sender,
            lead_frames,
        }
    }

    pub fn shared_sender(&self) -> SharedRealtimeMediaSender {
        Arc::clone(&self.sender)
    }
}

impl MsaWindowsPacketSink for RealtimeMsaPacketSink {
    fn can_accept_packet(&mut self, now_ntp: u64) -> bool {
        self.sender
            .lock()
            .map(|mut sender| sender.can_accept_frames(now_ntp))
            .unwrap_or(false)
    }

    fn send_packet(&mut self, pcm: &[u8], now_ntp: u64) -> Result<(), String> {
        self.sender
            .lock()
            .map_err(|_| "MSA realtime sender lock poisoned".to_owned())?
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
    paused: Arc<AtomicBool>,
    in_flight: Arc<AtomicBool>,
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
        let paused = Arc::new(AtomicBool::new(false));
        let paused_thread = Arc::clone(&paused);
        let in_flight = Arc::new(AtomicBool::new(false));
        let in_flight_thread = Arc::clone(&in_flight);

        let worker = thread::spawn(move || {
            while running_thread.load(Ordering::SeqCst) {
                if paused_thread.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                // Publish in-flight before touching sink/source, then re-check
                // pause to close the race with lifecycle quiesce.
                in_flight_thread.store(true, Ordering::SeqCst);
                if paused_thread.load(Ordering::SeqCst) {
                    in_flight_thread.store(false, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                    Ok(value) => value,
                    Err(error) => {
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some(format!("NTP clock conversion failed: {error:?}"));
                        }
                        in_flight_thread.store(false, Ordering::SeqCst);
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                if !sink.can_accept_packet(now_ntp) {
                    in_flight_thread.store(false, Ordering::SeqCst);
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
                            in_flight_thread.store(false, Ordering::SeqCst);
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    },
                    Err(_) => {
                        if let Ok(mut slot) = last_error_thread.lock() {
                            *slot = Some("MSA Windows source lock poisoned".into());
                        }
                        in_flight_thread.store(false, Ordering::SeqCst);
                        running_thread.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let Some(packet) = packet else {
                    // IDLE/STANDBY or temporary PLAYING starvation: never infer
                    // a state change and never synthesize a new anchor here.
                    in_flight_thread.store(false, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(1));
                    continue;
                };

                if let Err(error) = sink.send_packet(&packet, now_ntp) {
                    if let Ok(mut slot) = last_error_thread.lock() {
                        *slot = Some(format!("MSA media send failed: {error}"));
                    }
                    in_flight_thread.store(false, Ordering::SeqCst);
                    running_thread.store(false, Ordering::SeqCst);
                    return;
                }

                packets_sent_thread.fetch_add(1, Ordering::SeqCst);
                in_flight_thread.store(false, Ordering::SeqCst);
            }
        });

        Self {
            running,
            worker: Some(worker),
            last_error,
            packets_sent,
            paused,
            in_flight,
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

    /// Stop the send loop at a packet boundary and wait until any packet
    /// already inside the sink/source critical section has completed.
    pub fn quiesce(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);

        while self.in_flight.load(Ordering::SeqCst) {
            if !self.running.load(Ordering::SeqCst) {
                return Err("MSA Windows consumer stopped while quiescing".into());
            }
            if Instant::now() >= deadline {
                return Err("MSA Windows consumer quiesce timed out".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn is_quiesced(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
            && !self.in_flight.load(Ordering::SeqCst)
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

    #[test]
    fn quiesce_freezes_packet_count_until_resume() {
        let source = Arc::new(Mutex::new(
            MsaWindowsSource::new(
                44_100 * 2 * 2,
                WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
            ).unwrap(),
        ));
        {
            let mut locked = source.lock().unwrap();
            locked.push_capture_pcm(
                &vec![7u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO * 32],
            );
            locked.core_mut().start_committed().unwrap();
        }

        let sends = Arc::new(AtomicUsize::new(0));
        let mut worker = MsaWindowsConsumerWorker::start(
            Arc::clone(&source),
            Box::new(FakeSink { sends: Arc::clone(&sends) }),
        );

        for _ in 0..100 {
            if sends.load(Ordering::SeqCst) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        worker.quiesce().unwrap();
        assert!(worker.is_quiesced());
        let frozen = sends.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(sends.load(Ordering::SeqCst), frozen);

        worker.resume();
        for _ in 0..100 {
            if sends.load(Ordering::SeqCst) > frozen {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        worker.stop();
        assert!(sends.load(Ordering::SeqCst) > frozen);
    }


    #[test]
    fn realtime_sink_and_lifecycle_share_one_sender_instance() {
        use crate::{MediaTransport, RtpState};
        use std::net::{IpAddr, Ipv4Addr, UdpSocket};

        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        transport.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            crate::StreamPorts {
                data_port: data_rx.local_addr().unwrap().port(),
                control_port: ctrl_rx.local_addr().unwrap().port(),
            },
        );

        let sender = RealtimeMediaSender::new(
            transport,
            RtpState::new(10, 20, 30),
            [0x33u8; 32],
        );
        let shared = Arc::new(Mutex::new(sender));
        let sink = RealtimeMsaPacketSink::from_shared(Arc::clone(&shared), 11_025);

        assert!(Arc::ptr_eq(&shared, &sink.shared_sender()));
        assert_eq!(shared.lock().unwrap().state().sequence, 10);
        assert_eq!(shared.lock().unwrap().state().timestamp, 20);
    }

}
