use crate::{EncryptedRtspError, RtspRequest, SharedCseq, SharedRtspControl};
use std::sync::atomic::Ordering;
use std::sync::TryLockError;
use std::thread;
use std::time::{Duration, Instant};

const CONTROL_TIMEOUT: Duration = Duration::from_millis(2000);
const FAREWELL_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum TeardownError {
    Lock,
    LockTimeout,
    Transport(EncryptedRtspError),
}

fn request(
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
) -> RtspRequest {
    RtspRequest {
        method: "TEARDOWN".into(),
        uri: session_uri.into(),
        cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: dacp_id.into(),
        active_remote: active_remote.into(),
        client_instance: None,
        content_type: None,
        body: Vec::new(),
    }
}

pub fn send_teardown(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
) -> Result<(), TeardownError> {
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    let mut channel = loop {
        match control.try_lock() {
            Ok(channel) => break channel,
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(TeardownError::LockTimeout);
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Poisoned(_)) => return Err(TeardownError::Lock),
        }
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(TeardownError::LockTimeout);
    }

    // Match source: CSeq advances only once the serialized request is
    // actually about to start, after acquiring the control lock.
    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let req = request(cseq, session_uri, dacp_id, active_remote).encode();

    match channel.exchange_with_timeout(&req, cseq, remaining) {
        Ok(_) => Ok(()),
        Err(EncryptedRtspError::Timeout) => {
            // Match upstream farewell behavior: the read direction just failed,
            // so append one final TEARDOWN with a 250 ms write budget and do not
            // wait for its response.
            let farewell_cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
            let farewell = request(
                farewell_cseq,
                session_uri,
                dacp_id,
                active_remote,
            )
            .encode();
            channel
                .write_only_with_timeout(&farewell, FAREWELL_TIMEOUT)
                .map_err(TeardownError::Transport)
        }
        Err(error) => Err(TeardownError::Transport(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_request_has_source_headers_only() {
        let text = String::from_utf8(request(
            9,
            "rtsp://192.168.1.2/123",
            "AABBCCDDEEFF0011",
            "123456789",
        ).encode()).unwrap();

        assert!(text.starts_with("TEARDOWN rtsp://192.168.1.2/123 RTSP/1.0\r\n"));
        assert!(text.contains("CSeq: 9\r\n"));
        assert!(!text.contains("Client-Instance:"));
        assert!(text.ends_with("Content-Length: 0\r\n\r\n"));
    }
}
