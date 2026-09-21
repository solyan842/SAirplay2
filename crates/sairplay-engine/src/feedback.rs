use crate::{EncryptedRtspChannel, EncryptedRtspError, RtspRequest};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex, TryLockError,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const FEEDBACK_INTERVAL: Duration = Duration::from_millis(2000);
pub const FEEDBACK_TIMEOUT: Duration = Duration::from_millis(2000);
pub const MAX_CONSECUTIVE_MISSES: u32 = 3;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub type SharedRtspControl = Arc<Mutex<EncryptedRtspChannel>>;
pub type SharedCseq = Arc<AtomicU32>;

pub struct FeedbackWorker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
}

impl FeedbackWorker {
    pub fn start(
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        dacp_id: String,
        active_remote: String,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let last_error = Arc::new(Mutex::new(None));

        let stop_thread = Arc::clone(&stop);
        let running_thread = Arc::clone(&running);
        let error_thread = Arc::clone(&last_error);

        let worker = thread::Builder::new()
            .name("sairplay-feedback".into())
            .spawn(move || {
                let mut misses = 0u32;
                let mut next_tick = Instant::now() + FEEDBACK_INTERVAL;

                while !stop_thread.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now < next_tick {
                        thread::sleep((next_tick - now).min(STOP_POLL_INTERVAL));
                        continue;
                    }

                    // Source uses one total 2 s budget for lock acquisition +
                    // request/response. If the serialization lock consumes the
                    // whole budget, this tick is SKIPPED: no CSeq/nonce is
                    // consumed and it is not a receiver miss.
                    let deadline = Instant::now() + FEEDBACK_TIMEOUT;
                    let result_and_cseq = {
                        let mut channel = loop {
                            match control.try_lock() {
                                Ok(channel) => break Some(channel),
                                Err(TryLockError::WouldBlock) => {
                                    if stop_thread.load(Ordering::SeqCst) {
                                        break None;
                                    }
                                    if Instant::now() >= deadline {
                                        break None;
                                    }
                                    thread::sleep(Duration::from_millis(5));
                                }
                                Err(TryLockError::Poisoned(_)) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some("RTSP control mutex poisoned".into());
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        };

                        if stop_thread.load(Ordering::SeqCst) {
                            break;
                        }

                        let Some(ref mut channel) = channel else {
                            // Busy control path: source calls this a skipped tick.
                            next_tick = Instant::now() + FEEDBACK_INTERVAL;
                            continue;
                        };

                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            next_tick = Instant::now() + FEEDBACK_INTERVAL;
                            continue;
                        }

                        // CSeq advances only once the request is actually starting.
                        let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
                        let request = RtspRequest {
                            method: "POST".into(),
                            uri: "/feedback".into(),
                            cseq,
                            user_agent: "AirPlay/670.6.2".into(),
                            dacp_id: dacp_id.clone(),
                            active_remote: active_remote.clone(),
                            client_instance: None,
                            content_type: None,
                            body: Vec::new(),
                        };

                        let result = channel.exchange_with_timeout(
                            &request.encode(),
                            cseq,
                            remaining,
                        );
                        (result, cseq)
                    };

                    let (result, cseq) = result_and_cseq;
                    match result {
                        Ok(response) if response.status == 200 => {
                            misses = 0;
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = None;
                            }
                        }
                        Ok(response) => {
                            misses = misses.saturating_add(1);
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(format!(
                                    "/feedback CSeq {cseq} returned RTSP {} (miss {misses}/{MAX_CONSECUTIVE_MISSES})",
                                    response.status
                                ));
                            }
                        }
                        Err(error) => {
                            // Pinned source only tolerates timeout-shaped
                            // /feedback failures. A hard peer/channel error
                            // means the RTSP connection is already gone and is
                            // terminal immediately.
                            if matches!(error, EncryptedRtspError::Timeout) {
                                misses = misses.saturating_add(1);
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!(
                                        "/feedback CSeq {cseq} timed out (miss {misses}/{MAX_CONSECUTIVE_MISSES})"
                                    ));
                                }
                            } else {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!(
                                        "/feedback CSeq {cseq} hard failure: {error:?} · peer/control channel closed"
                                    ));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                    }

                    if misses >= MAX_CONSECUTIVE_MISSES {
                        break;
                    }

                    let after = Instant::now();
                    next_tick += FEEDBACK_INTERVAL;
                    if next_tick <= after {
                        next_tick = after + FEEDBACK_INTERVAL;
                    }
                }

                running_thread.store(false, Ordering::SeqCst);
            })?;

        Ok(Self {
            stop,
            running,
            last_error,
            worker: Some(worker),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for FeedbackWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HapControlCipher;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn read_frame(socket: &mut TcpStream, cipher: &mut HapControlCipher) -> Vec<u8> {
        let mut carry = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            let n = socket.read(&mut buf).unwrap();
            carry.extend_from_slice(&buf[..n]);
            if carry.len() < 2 {
                continue;
            }
            let plen = u16::from_le_bytes([carry[0], carry[1]]) as usize;
            let total = 2 + plen + 16;
            if carry.len() >= total {
                return cipher.decrypt(&carry[..total]).unwrap();
            }
        }
    }

    #[test]
    fn only_timeout_is_a_tolerable_feedback_transport_error() {
        assert!(matches!(
            EncryptedRtspError::Timeout,
            EncryptedRtspError::Timeout
        ));
        assert!(!matches!(
            EncryptedRtspError::Closed,
            EncryptedRtspError::Timeout
        ));
        let write = EncryptedRtspError::Write(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "closed",
        ));
        assert!(!matches!(write, EncryptedRtspError::Timeout));
    }

    #[test]
    fn feedback_request_matches_source_shape_and_shared_cseq() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let key = [0x44u8; 32];

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
            let mut cipher = HapControlCipher::new(key, key);

            let plain = read_frame(&mut socket, &mut cipher);
            let text = String::from_utf8(plain).unwrap();
            assert!(text.starts_with("POST /feedback RTSP/1.0\r\n"));
            assert!(text.contains("CSeq: 4\r\n"));
            assert!(text.contains("Content-Length: 0\r\n\r\n"));

            let response = b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nContent-Length: 0\r\n\r\n";
            let wire = cipher.encrypt(response).unwrap();
            socket.write_all(&wire).unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let control = Arc::new(Mutex::new(EncryptedRtspChannel::new(
            stream,
            key,
            key,
            Duration::from_secs(8),
        )));
        let cseq = Arc::new(AtomicU32::new(4));

        let mut worker = FeedbackWorker::start(
            Arc::clone(&control),
            Arc::clone(&cseq),
            "AABBCCDDEEFF0011".into(),
            "123".into(),
        )
        .unwrap();

        thread::sleep(Duration::from_millis(2300));
        worker.stop();

        assert_eq!(cseq.load(Ordering::SeqCst), 5);
        server.join().unwrap();
    }
}
