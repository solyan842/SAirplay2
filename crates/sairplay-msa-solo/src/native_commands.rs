//! Native AP2 SOLO RTSP media control verbs from pinned MSA.

use crate::{EncryptedRtspChannel, EncryptedRtspError, RtspRequest};
use crate::ptp_engine::PtpClock;
use crate::ntp_timing::system_time_to_ntp;
use plist::{Dictionary, Value};
use std::io::Cursor;
use std::time::{Duration, SystemTime};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

#[derive(Debug)]
pub enum NativeCommandError {
    Transport(EncryptedRtspError),
    Plist(plist::Error),
    Status(u16),
    Time,
    AnchorRetriesExhausted,
}
impl From<EncryptedRtspError> for NativeCommandError {
    fn from(v: EncryptedRtspError) -> Self { Self::Transport(v) }
}
impl From<plist::Error> for NativeCommandError {
    fn from(v: plist::Error) -> Self { Self::Plist(v) }
}

fn request_with_extra_headers(
    method: &str,
    uri: &str,
    cseq: u32,
    dacp_id: &str,
    active_remote: &str,
    content_type: Option<&str>,
    extra_headers: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut head = format!(
        "{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: AirPlay/670.6.2\r\nDACP-ID: {dacp_id}\r\nActive-Remote: {active_remote}\r\n"
    );
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str(extra_headers);
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

pub fn send_realtime_flush(
    channel: &mut EncryptedRtspChannel,
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    seq: u16,
    rtp: u32,
) -> Result<(), NativeCommandError> {
    let extra = format!("RTP-Info: seq={seq};rtptime={rtp}\r\n");
    let req = request_with_extra_headers(
        "FLUSH", session_uri, cseq, dacp_id, active_remote, None, &extra, &[],
    );
    let response = channel.exchange(&req, cseq)?;
    if response.status != 200 {
        return Err(NativeCommandError::Status(response.status));
    }
    Ok(())
}

pub fn build_flushbuffered_plist(seq: u16, rtp: u32) -> Result<Vec<u8>, NativeCommandError> {
    let mut root = Dictionary::new();
    root.insert("flushUntilSeq".into(), Value::Integer((seq as u64).into()));
    root.insert("flushUntilTS".into(), Value::Integer((rtp as u64).into()));
    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

pub fn send_flushbuffered(
    channel: &mut EncryptedRtspChannel,
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    seq: u16,
    rtp: u32,
) -> Result<(), NativeCommandError> {
    let body = build_flushbuffered_plist(seq, rtp)?;
    let request = RtspRequest {
        method: "FLUSHBUFFERED".into(),
        uri: session_uri.into(),
        cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: dacp_id.into(),
        active_remote: active_remote.into(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };
    let response = channel.exchange(&request.encode(), cseq)?;
    if response.status != 200 {
        return Err(NativeCommandError::Status(response.status));
    }
    Ok(())
}

pub fn build_setrateanchortime_plist(
    clock_id: u64,
    rtp_time: u32,
    anchor_ns: u64,
    rate: u64,
) -> Result<Vec<u8>, NativeCommandError> {
    let secs = anchor_ns / 1_000_000_000;
    let rem_ns = anchor_ns % 1_000_000_000;
    let frac32 = ((rem_ns as u128) << 32) / 1_000_000_000u128;
    let network_time_frac = (frac32 as u64) << 32;

    let mut root = Dictionary::new();
    root.insert("networkTimeTimelineID".into(), Value::Integer(clock_id.into()));
    root.insert("networkTimeSecs".into(), Value::Integer(secs.into()));
    root.insert("networkTimeFrac".into(), Value::Integer(network_time_frac.into()));
    root.insert("rtpTime".into(), Value::Integer((rtp_time as u64).into()));
    root.insert("rate".into(), Value::Integer(rate.into()));

    let mut out = Vec::new();
    Value::Dictionary(root).to_writer_binary(&mut out)?;
    Ok(out)
}

pub fn send_setrateanchortime(
    channel: &mut EncryptedRtspChannel,
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    clock: &PtpClock,
    rtp_time: u32,
    anchor_ns: u64,
    rate: u64,
) -> Result<(), NativeCommandError> {
    let body = build_setrateanchortime_plist(
        clock.master_clock_id(), rtp_time, anchor_ns, rate,
    )?;
    let request = RtspRequest {
        method: "SETRATEANCHORTIME".into(),
        uri: session_uri.into(),
        cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: dacp_id.into(),
        active_remote: active_remote.into(),
        client_instance: None,
        content_type: Some("application/x-apple-binary-plist".into()),
        body,
    };
    let response = channel.exchange(&request.encode(), cseq)?;
    if response.status != 200 {
        return Err(NativeCommandError::Status(response.status));
    }
    Ok(())
}

fn remaining_lead_ns(commanded_start_ntp: u64, now_ntp: u64) -> u64 {
    if commanded_start_ntp <= now_ntp { return 0; }
    let d = commanded_start_ntp - now_ntp;
    (d >> 32).saturating_mul(1_000_000_000)
        .saturating_add((((d & 0xffff_ffff) as u128 * 1_000_000_000u128) >> 32) as u64)
}

pub fn buffered_anchor_start(
    channel: &mut EncryptedRtspChannel,
    next_cseq: &AtomicU32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    clock: &PtpClock,
    rtp_time: u32,
    commanded_start_ntp: u64,
) -> Result<u64, NativeCommandError> {
    for attempt in 0..12 {
        if attempt != 0 { thread::sleep(Duration::from_millis(500)); }
        let now_ntp = system_time_to_ntp(SystemTime::now()).map_err(|_| NativeCommandError::Time)?;
        let lead_ns = remaining_lead_ns(commanded_start_ntp, now_ntp);
        let anchor_ns = clock.master_now_ns().saturating_add(lead_ns);
        let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
        if send_setrateanchortime(
            channel, cseq, session_uri, dacp_id, active_remote,
            clock, rtp_time, anchor_ns, 1,
        ).is_ok() {
            return Ok(anchor_ns);
        }
    }
    Err(NativeCommandError::AnchorRetriesExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushbuffered_plist_names_current_wire_boundary() {
        let body = build_flushbuffered_plist(0x1234, 0xaabbccdd).unwrap();
        let value = Value::from_reader(Cursor::new(body)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(root.get("flushUntilSeq").and_then(Value::as_unsigned_integer), Some(0x1234));
        assert_eq!(root.get("flushUntilTS").and_then(Value::as_unsigned_integer), Some(0xaabbccdd));
    }

    #[test]
    fn rate_anchor_fraction_matches_pinned_msa() {
        let anchor_ns = 12_345_678_901u64;
        let body = build_setrateanchortime_plist(7, 9, anchor_ns, 1).unwrap();
        let value = Value::from_reader(Cursor::new(body)).unwrap();
        let root = value.as_dictionary().unwrap();
        let frac32 = (((345_678_901u128) << 32) / 1_000_000_000u128) as u64;
        assert_eq!(root.get("networkTimeFrac").and_then(Value::as_unsigned_integer), Some(frac32 << 32));
    }

    #[test]
    fn realtime_flush_has_exact_rtp_info_header() {
        let req = request_with_extra_headers(
            "FLUSH", "rtsp://127.0.0.1/1", 5, "AA", "1", None,
            "RTP-Info: seq=7;rtptime=55\r\n", &[],
        );
        let text = String::from_utf8(req).unwrap();
        assert!(text.contains("RTP-Info: seq=7;rtptime=55\r\n"));
        assert!(text.ends_with("Content-Length: 0\r\n\r\n"));
    }
}
