use crate::{EncryptedRtspChannel, RtspRequest};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
const MAX_CONSECUTIVE_MISSES: u32 = 3;

pub struct FeedbackWorker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<EncryptedRtspChannel>>,
}

impl FeedbackWorker {
    pub fn start(
        mut channel: EncryptedRtspChannel,
        dacp_id: String,
        active_remote: String,
        first_cseq: u32,
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
                let mut cseq = first_cseq;
                let mut misses = 0u32;

                while !stop_thread.load(Ordering::SeqCst) {
                    thread::sleep(FEEDBACK_INTERVAL);
                    if stop_thread.load(Ordering::SeqCst) {
                        break;
                    }

                    let request = RtspRequest {
                        method: "POST".into(),
                        uri: "/feedback".into(),
                        cseq,
                        user_agent: "AirPlay/670.6.2".into(),
                        dacp_id: dacp_id.clone(),
                        active_remote: active_remote.clone(),
                        client_instance: Some(dacp_id.clone()),
                        content_type: None,
                        body: Vec::new(),
                    };

                    match channel.exchange(&request.encode(), cseq) {
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
                            misses = misses.saturating_add(1);
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(format!(
                                    "/feedback CSeq {cseq} failed: {error:?} (miss {misses}/{MAX_CONSECUTIVE_MISSES})"
                                ));
                            }
                        }
                    }

                    cseq = cseq.wrapping_add(1);
                    if misses >= MAX_CONSECUTIVE_MISSES {
                        break;
                    }
                }

                running_thread.store(false, Ordering::SeqCst);
                channel
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

    pub fn stop(&mut self) -> Option<EncryptedRtspChannel> {
        self.stop.store(true, Ordering::SeqCst);
        let channel = self.worker.take().and_then(|worker| worker.join().ok());
        self.running.store(false, Ordering::SeqCst);
        channel
    }
}

impl Drop for FeedbackWorker {
    fn drop(&mut self) {
        let _ = self.stop();
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
            if carry.len() < 2 { continue; }
            let plen = u16::from_le_bytes([carry[0], carry[1]]) as usize;
            let total = 2 + plen + 16;
            if carry.len() >= total {
                return cipher.decrypt(&carry[..total]).unwrap();
            }
        }
    }

    #[test]
    fn feedback_request_shape_is_post_empty_body_and_monotonic_cseq() {
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
        let channel = EncryptedRtspChannel::new(stream, key, key, Duration::from_secs(1));
        let mut worker = FeedbackWorker::start(channel, "AABBCCDDEEFF0011".into(), "123".into(), 4).unwrap();
        thread::sleep(Duration::from_millis(2300));
        let _ = worker.stop();
        server.join().unwrap();
    }
}
