use crate::{EncryptedRtspError, RtspRequest, SharedCseq, SharedRtspControl};
use std::sync::atomic::Ordering;
use std::sync::TryLockError;
use std::thread;
use std::time::{Duration, Instant};

const VOLUME_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeSetResult {
    pub percent: u8,
    pub db: f32,
    pub status: u16,
}

#[derive(Debug)]
pub enum VolumeError {
    Lock,
    LockTimeout,
    Transport(EncryptedRtspError),
}

pub fn volume_percent_to_db(percent: u8) -> f32 {
    let percent = percent.min(100);
    if percent == 0 {
        -144.0
    } else {
        // Exact libraop raopcl_float_volume() mapping:
        // VOLUME_MIN + ((VOLUME_MAX - VOLUME_MIN) * vol) / 100,
        // with VOLUME_MIN=-30 and VOLUME_MAX=0.
        -30.0 + (30.0 * percent as f32) / 100.0
    }
}

fn request(
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    db: f32,
) -> RtspRequest {
    RtspRequest {
        method: "SET_PARAMETER".into(),
        uri: session_uri.into(),
        cseq,
        user_agent: "AirPlay/670.6.2".into(),
        dacp_id: dacp_id.into(),
        active_remote: active_remote.into(),
        client_instance: None,
        content_type: Some("text/parameters".into()),
        body: format!("volume: {db:.6}\r\n").into_bytes(),
    }
}

#[derive(Clone)]
pub struct NativeVolumeControl {
    control: SharedRtspControl,
    next_cseq: SharedCseq,
    session_uri: String,
    dacp_id: String,
    active_remote: String,
}

impl NativeVolumeControl {
    pub(crate) fn new(
        control: SharedRtspControl,
        next_cseq: SharedCseq,
        session_uri: String,
        dacp_id: String,
        active_remote: String,
    ) -> Self {
        Self {
            control,
            next_cseq,
            session_uri,
            dacp_id,
            active_remote,
        }
    }

    pub fn set(&self, percent: u8) -> Result<VolumeSetResult, VolumeError> {
        set_native_volume(
            &self.control,
            &self.next_cseq,
            &self.session_uri,
            &self.dacp_id,
            &self.active_remote,
            percent,
        )
    }
}

pub fn set_native_volume(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    percent: u8,
) -> Result<VolumeSetResult, VolumeError> {
    let percent = percent.min(100);
    let db = volume_percent_to_db(percent);
    let deadline = Instant::now() + VOLUME_TIMEOUT;

    let mut channel = loop {
        match control.try_lock() {
            Ok(channel) => break channel,
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(VolumeError::LockTimeout);
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Poisoned(_)) => return Err(VolumeError::Lock),
        }
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(VolumeError::LockTimeout);
    }

    // Match source RTSP serialization semantics: CSeq advances only when
    // the request is actually about to start after the lock is acquired.
    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let req = request(cseq, session_uri, dacp_id, active_remote, db).encode();
    let response = channel
        .exchange_with_timeout(&req, cseq, remaining)
        .map_err(VolumeError::Transport)?;

    Ok(VolumeSetResult {
        percent,
        db,
        status: response.status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_mapping_matches_libraop_exactly() {
        assert_eq!(volume_percent_to_db(0), -144.0);
        assert!((volume_percent_to_db(1) - -29.7).abs() < 0.0001);
        assert_eq!(volume_percent_to_db(50), -15.0);
        assert_eq!(volume_percent_to_db(100), 0.0);
    }

    #[test]
    fn request_matches_native_source_shape() {
        let text = String::from_utf8(
            request(
                7,
                "rtsp://192.168.1.2/123",
                "AABBCCDDEEFF0011",
                "123456789",
                -15.0,
            )
            .encode(),
        )
        .unwrap();

        assert!(text.starts_with(
            "SET_PARAMETER rtsp://192.168.1.2/123 RTSP/1.0\r\n"
        ));
        assert!(text.contains("CSeq: 7\r\n"));
        assert!(text.contains("Content-Type: text/parameters\r\n"));
        assert!(!text.contains("Client-Instance:"));
        assert!(text.ends_with("volume: -15.000000\r\n"));
    }
}
