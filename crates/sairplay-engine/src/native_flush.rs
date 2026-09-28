//! Source-accurate native AP2 realtime FLUSH primitive.
//!
//! Mirrors pinned airplay-cli's stock native realtime path:
//!   FLUSH <session-uri>
//!   RTP-Info: seq=<current>;rtptime=<current>
//! A 200 response is the only successful receiver acknowledgement.
//! The session/control channel stays alive; this helper never TEARDOWNs.

use crate::{EncryptedRtspError, SharedCseq, SharedRtspControl};
use std::fmt;
use std::sync::atomic::Ordering;
use std::time::Duration;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum NativeFlushError {
    Lock,
    Transport(EncryptedRtspError),
    Status(u16),
}

impl fmt::Display for NativeFlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock => write!(f, "native FLUSH control lock poisoned"),
            Self::Transport(error) => write!(f, "native FLUSH transport failed: {error}"),
            Self::Status(status) => write!(f, "native FLUSH rejected with RTSP {status}"),
        }
    }
}

impl std::error::Error for NativeFlushError {}

fn build_native_flush_request(
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    sequence: u16,
    rtptime: u32,
) -> Vec<u8> {
    format!(
        "FLUSH {session_uri} RTSP/1.0\r\n\
CSeq: {cseq}\r\n\
User-Agent: AirPlay/670.6.2\r\n\
DACP-ID: {dacp_id}\r\n\
Active-Remote: {active_remote}\r\n\
RTP-Info: seq={sequence};rtptime={rtptime}\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

pub fn send_native_realtime_flush(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    sequence: u16,
    rtptime: u32,
) -> Result<(), NativeFlushError> {
    let mut channel = control.lock().map_err(|_| NativeFlushError::Lock)?;

    // Match existing serialized-control ownership: CSeq advances only after
    // the RTSP control lock has been acquired.
    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let request = build_native_flush_request(
        cseq,
        session_uri,
        dacp_id,
        active_remote,
        sequence,
        rtptime,
    );

    let response = channel
        .exchange_with_timeout(&request, cseq, FLUSH_TIMEOUT)
        .map_err(NativeFlushError::Transport)?;

    if response.status != 200 {
        return Err(NativeFlushError::Status(response.status));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_request_matches_native_ap2_realtime_shape() {
        let text = String::from_utf8(build_native_flush_request(
            9,
            "rtsp://192.168.1.2/123",
            "AABBCCDDEEFF0011",
            "123456789",
            321,
            456_789,
        ))
        .unwrap();

        assert!(text.starts_with(
            "FLUSH rtsp://192.168.1.2/123 RTSP/1.0\r\n"
        ));
        assert!(text.contains("CSeq: 9\r\n"));
        assert!(text.contains("User-Agent: AirPlay/670.6.2\r\n"));
        assert!(text.contains("DACP-ID: AABBCCDDEEFF0011\r\n"));
        assert!(text.contains("Active-Remote: 123456789\r\n"));
        assert!(text.contains("RTP-Info: seq=321;rtptime=456789\r\n"));
        assert!(text.ends_with("Content-Length: 0\r\n\r\n"));
        assert!(!text.contains("TEARDOWN"));
    }
}
