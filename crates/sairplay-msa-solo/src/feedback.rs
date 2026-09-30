use crate::{write_farewell_teardown_locked, EncryptedRtspChannel, EncryptedRtspError, RtspRequest};
use plist::Value;
use std::io::Cursor;
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

fn feedback_transport_error_is_timeout(error: &EncryptedRtspError) -> bool {
    matches!(error, EncryptedRtspError::Timeout)
}

pub struct FeedbackWorker {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    misses: Arc<AtomicU32>,
    last_error: Arc<Mutex<Option<String>>>,
    last_stream_count: Arc<Mutex<Option<usize>>>,
    worker: Option<JoinHandle<()>>,
}

impl FeedbackWorker {
    pub fn start(
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        dacp_id: String,
        active_remote: String,
        session_uri: String,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let misses = Arc::new(AtomicU32::new(0));
        let last_error = Arc::new(Mutex::new(None));
        let last_stream_count = Arc::new(Mutex::new(None));

        let stop_thread = Arc::clone(&stop);
        let running_thread = Arc::clone(&running);
        let misses_thread = Arc::clone(&misses);
        let error_thread = Arc::clone(&last_error);
        let streams_thread = Arc::clone(&last_stream_count);

        let worker = thread::Builder::new()
            .name("sairplay-msa-feedback".into())
            .spawn(move || {
                let mut next_tick = Instant::now() + FEEDBACK_INTERVAL;

                while !stop_thread.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now < next_tick {
                        thread::sleep((next_tick - now).min(STOP_POLL_INTERVAL));
                        continue;
                    }

                    // Pinned MSA: one 2s budget covers control-lock acquisition
                    // plus the /feedback request/response. Expiring before the
                    // request begins is SKIPPED and consumes neither CSeq nor
                    // HAP nonce and is not a receiver miss.
                    let deadline = Instant::now() + FEEDBACK_TIMEOUT;
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
                        next_tick = Instant::now() + FEEDBACK_INTERVAL;
                        continue;
                    };

                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        next_tick = Instant::now() + FEEDBACK_INTERVAL;
                        continue;
                    }

                    // CSeq advances only once the request actually starts.
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

                    match result {
                        Ok(response) if response.status == 200 => {
                            misses_thread.store(0, Ordering::SeqCst);
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = None;
                            }
                            if let Ok(mut slot) = streams_thread.lock() {
                                *slot = feedback_stream_count(&response.body);
                            }
                        }
                        Ok(response) => {
                            let now_misses = misses_thread.fetch_add(1, Ordering::SeqCst) + 1;
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(format!(
                                    "/feedback CSeq {cseq} returned RTSP {} (miss {now_misses}/{MAX_CONSECUTIVE_MISSES})",
                                    response.status
                                ));
                            }
                        }
                        Err(error) if feedback_transport_error_is_timeout(&error) => {
                            let now_misses = misses_thread.fetch_add(1, Ordering::SeqCst) + 1;
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(format!(
                                    "/feedback CSeq {cseq} timed out (miss {now_misses}/{MAX_CONSECUTIVE_MISSES})"
                                ));
                            }
                            if now_misses >= MAX_CONSECUTIVE_MISSES {
                                // Pinned MSA appends one final encrypted TEARDOWN
                                // on a timeout-dead but still intact control stream.
                                let _ = write_farewell_teardown_locked(
                                    channel,
                                    &next_cseq,
                                    &session_uri,
                                    &dacp_id,
                                    &active_remote,
                                );
                            }
                        }
                        Err(error) => {
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(format!(
                                    "/feedback CSeq {cseq} hard failure: {error:?}"
                                ));
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            return;
                        }
                    }

                    drop(channel);
                    if misses_thread.load(Ordering::SeqCst) >= MAX_CONSECUTIVE_MISSES {
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
            misses,
            last_error,
            last_stream_count,
            worker: Some(worker),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn consecutive_misses(&self) -> u32 {
        self.misses.load(Ordering::SeqCst)
    }

    pub fn healthy(&self) -> bool {
        self.is_running() && self.consecutive_misses() < MAX_CONSECUTIVE_MISSES
    }

    pub fn last_stream_count(&self) -> Option<usize> {
        self.last_stream_count.lock().ok().and_then(|v| *v)
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


fn feedback_stream_count(body: &[u8]) -> Option<usize> {
    if body.is_empty() {
        return None;
    }
    let root = Value::from_reader(Cursor::new(body)).ok()?;
    let dict = root.as_dictionary()?;
    match dict.get("streams")? {
        Value::Array(v) => Some(v.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_feedback_constants_are_exact() {
        assert_eq!(FEEDBACK_INTERVAL, Duration::from_millis(2000));
        assert_eq!(FEEDBACK_TIMEOUT, Duration::from_millis(2000));
        assert_eq!(MAX_CONSECUTIVE_MISSES, 3);
    }

    #[test]
    fn feedback_stream_count_distinguishes_empty_active_and_missing() {
        fn plist_with_streams(count: usize) -> Vec<u8> {
            let mut d = plist::Dictionary::new();
            d.insert("streams".into(), Value::Array((0..count).map(|_| Value::Dictionary(plist::Dictionary::new())).collect()));
            let mut out = Vec::new();
            Value::Dictionary(d).to_writer_binary(&mut out).unwrap();
            out
        }
        assert_eq!(feedback_stream_count(&plist_with_streams(0)), Some(0));
        assert_eq!(feedback_stream_count(&plist_with_streams(1)), Some(1));
        let mut d = plist::Dictionary::new();
        d.insert("stream".into(), Value::Array(Vec::new()));
        let mut out = Vec::new();
        Value::Dictionary(d).to_writer_binary(&mut out).unwrap();
        assert_eq!(feedback_stream_count(&out), None);
        assert_eq!(feedback_stream_count(&out[..8]), None);
    }

    #[test]
    fn only_timeout_is_tolerable_transport_failure() {
        assert!(feedback_transport_error_is_timeout(&EncryptedRtspError::Timeout));
        assert!(!feedback_transport_error_is_timeout(&EncryptedRtspError::Closed));
    }
}
