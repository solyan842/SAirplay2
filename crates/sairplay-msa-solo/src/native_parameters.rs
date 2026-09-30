//! Native SOLO SET_PARAMETER controls from pinned MSA: artwork and progress.

use crate::{EncryptedRtspError, SharedCseq, SharedRtspControl};
use std::sync::{atomic::Ordering, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

const PARAM_TIMEOUT: Duration = Duration::from_millis(5000);
const ARTWORK_TIMEOUT: Duration = Duration::from_millis(15000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterResult {
    pub status: u16,
    pub bytes: usize,
}

#[derive(Debug)]
pub enum ParameterError {
    InvalidInput,
    Lock,
    LockTimeout,
    Transport(EncryptedRtspError),
}

fn lock_control(
    control: &SharedRtspControl,
    deadline: Instant,
) -> Result<std::sync::MutexGuard<'_, crate::EncryptedRtspChannel>, ParameterError> {
    loop {
        match control.try_lock() {
            Ok(channel) => return Ok(channel),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline { return Err(ParameterError::LockTimeout); }
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Poisoned(_)) => return Err(ParameterError::Lock),
        }
    }
}

fn request_with_rtp_info(
    method: &str,
    uri: &str,
    cseq: u32,
    dacp_id: &str,
    active_remote: &str,
    content_type: &str,
    rtp_timestamp: u32,
    body: &[u8],
) -> Vec<u8> {
    let mut out = format!(
        "{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: AirPlay/670.6.2\r\nDACP-ID: {dacp_id}\r\nActive-Remote: {active_remote}\r\nContent-Type: {content_type}\r\nRTP-Info: rtptime={rtp_timestamp}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ).into_bytes();
    out.extend_from_slice(body);
    out
}

fn text_request(
    uri: &str,
    cseq: u32,
    dacp_id: &str,
    active_remote: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut out = format!(
        "SET_PARAMETER {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: AirPlay/670.6.2\r\nDACP-ID: {dacp_id}\r\nActive-Remote: {active_remote}\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ).into_bytes();
    out.extend_from_slice(body);
    out
}

pub fn send_native_artwork(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    content_type: &str,
    data: &[u8],
    rtp_timestamp: u32,
) -> Result<ParameterResult, ParameterError> {
    if content_type.is_empty() || data.is_empty() { return Err(ParameterError::InvalidInput); }
    let deadline = Instant::now() + ARTWORK_TIMEOUT;
    let mut channel = lock_control(control, deadline)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() { return Err(ParameterError::LockTimeout); }
    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let req = request_with_rtp_info(
        "SET_PARAMETER", session_uri, cseq, dacp_id, active_remote,
        content_type, rtp_timestamp, data,
    );
    let response = channel.exchange_with_timeout(&req, cseq, remaining)
        .map_err(ParameterError::Transport)?;
    Ok(ParameterResult { status: response.status, bytes: data.len() })
}

/// Legacy progress path used only when MediaRemote is unavailable. Values are
/// expressed in the stream's RTP timestamp domain including rtp_offset.
pub fn send_native_progress(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    now_wire_rtp: u32,
    sample_rate: u32,
    elapsed_s: u32,
    duration_s: u32,
) -> Result<ParameterResult, ParameterError> {
    let start = now_wire_rtp.wrapping_sub(elapsed_s.saturating_mul(sample_rate));
    let end = if duration_s != 0 {
        start.wrapping_add(duration_s.saturating_mul(sample_rate))
    } else {
        now_wire_rtp
    };
    let body = format!("progress: {start}/{now_wire_rtp}/{end}\r\n").into_bytes();
    let deadline = Instant::now() + PARAM_TIMEOUT;
    let mut channel = lock_control(control, deadline)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() { return Err(ParameterError::LockTimeout); }
    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let req = text_request(session_uri, cseq, dacp_id, active_remote, &body);
    let response = channel.exchange_with_timeout(&req, cseq, remaining)
        .map_err(ParameterError::Transport)?;
    Ok(ParameterResult { status: response.status, bytes: body.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artwork_has_required_rtp_info() {
        let req=request_with_rtp_info(
            "SET_PARAMETER","rtsp://1/session",7,"AA","1","image/jpeg",0x1234,&[1,2,3]
        );
        let head=String::from_utf8(req[..req.windows(4).position(|w|w==b"\r\n\r\n").unwrap()+4].to_vec()).unwrap();
        assert!(head.contains("RTP-Info: rtptime=4660\r\n"));
        assert!(head.contains("Content-Type: image/jpeg\r\n"));
    }
    #[test]
    fn progress_uses_wrapping_wire_domain() {
        let now=1000u32; let sr=100u32; let elapsed=2u32; let duration=10u32;
        let start=now.wrapping_sub(elapsed*sr); let end=start.wrapping_add(duration*sr);
        assert_eq!((start,end),(800,1800));
    }
}
