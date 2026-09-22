use crate::{EncryptedRtspError, SharedCseq, SharedRtspControl};
use std::sync::atomic::Ordering;
use std::sync::TryLockError;
use std::thread;
use std::time::{Duration, Instant};

const METADATA_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataSetResult {
    pub status: u16,
    pub bytes: usize,
}

#[derive(Debug)]
pub enum MetadataError {
    Lock,
    LockTimeout,
    Transport(EncryptedRtspError),
}

#[derive(Clone)]
pub struct NativeMetadataControl {
    control: SharedRtspControl,
    next_cseq: SharedCseq,
    session_uri: String,
    dacp_id: String,
    active_remote: String,
}

impl NativeMetadataControl {
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

    pub fn send(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        rtp_timestamp: u32,
    ) -> Result<MetadataSetResult, MetadataError> {
        send_native_metadata(
            &self.control,
            &self.next_cseq,
            &self.session_uri,
            &self.dacp_id,
            &self.active_remote,
            title,
            artist,
            album,
            rtp_timestamp,
        )
    }
}

pub fn build_dmap_metadata(title: &str, artist: &str, album: &str) -> Vec<u8> {
    // Exact ap2_native_send_metadata() layout from the pinned primary source:
    // mlit { mikd=2, minm=<title>, asar=<artist>, asal=<album>, astn=1 }.
    let mut payload = Vec::with_capacity(
        9 + (8 + title.len()) + (8 + artist.len()) + (8 + album.len()) + 10,
    );

    payload.extend_from_slice(b"mikd");
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.push(2);

    for (tag, value) in [
        (b"minm".as_slice(), title.as_bytes()),
        (b"asar".as_slice(), artist.as_bytes()),
        (b"asal".as_slice(), album.as_bytes()),
    ] {
        payload.extend_from_slice(tag);
        payload.extend_from_slice(&(value.len() as u32).to_be_bytes());
        payload.extend_from_slice(value);
    }

    payload.extend_from_slice(b"astn");
    payload.extend_from_slice(&2u32.to_be_bytes());
    payload.extend_from_slice(&1u16.to_be_bytes());

    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(b"mlit");
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

fn request(
    cseq: u32,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    body: &[u8],
    rtp_timestamp: u32,
) -> Vec<u8> {
    // Native Sonos-class receivers require RTP-Info on DMAP metadata; the
    // primary source receives HTTP 400 without it.
    let mut out = format!(
        "SET_PARAMETER {session_uri} RTSP/1.0\r\n\
CSeq: {cseq}\r\n\
User-Agent: AirPlay/670.6.2\r\n\
DACP-ID: {dacp_id}\r\n\
Active-Remote: {active_remote}\r\n\
Content-Type: application/x-dmap-tagged\r\n\
RTP-Info: rtptime={rtp_timestamp}\r\n\
Content-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

pub fn send_native_metadata(
    control: &SharedRtspControl,
    next_cseq: &SharedCseq,
    session_uri: &str,
    dacp_id: &str,
    active_remote: &str,
    title: &str,
    artist: &str,
    album: &str,
    rtp_timestamp: u32,
) -> Result<MetadataSetResult, MetadataError> {
    let body = build_dmap_metadata(title, artist, album);
    let deadline = Instant::now() + METADATA_TIMEOUT;

    let mut channel = loop {
        match control.try_lock() {
            Ok(channel) => break channel,
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(MetadataError::LockTimeout);
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::Poisoned(_)) => return Err(MetadataError::Lock),
        }
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(MetadataError::LockTimeout);
    }

    let cseq = next_cseq.fetch_add(1, Ordering::SeqCst);
    let req = request(
        cseq,
        session_uri,
        dacp_id,
        active_remote,
        &body,
        rtp_timestamp,
    );
    let response = channel
        .exchange_with_timeout(&req, cseq, remaining)
        .map_err(MetadataError::Transport)?;

    Ok(MetadataSetResult {
        status: response.status,
        bytes: body.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_dmap_shape_contains_placeholder_track_fields() {
        let body = build_dmap_metadata("SAirplay2", "", "");
        assert!(body.starts_with(b"mlit"));
        assert!(body.windows(4).any(|w| w == b"mikd"));
        assert!(body.windows(4).any(|w| w == b"minm"));
        assert!(body.windows(4).any(|w| w == b"asar"));
        assert!(body.windows(4).any(|w| w == b"asal"));
        assert!(body.windows(4).any(|w| w == b"astn"));
    }

    #[test]
    fn native_metadata_request_has_required_rtp_info() {
        let body = build_dmap_metadata("SAirplay2", "", "");
        let req = request(
            9,
            "rtsp://192.168.1.2/123",
            "AABBCCDDEEFF0011",
            "123456789",
            &body,
            0x10203040,
        );
        let split = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8(req[..split].to_vec()).unwrap();
        assert!(head.starts_with(
            "SET_PARAMETER rtsp://192.168.1.2/123 RTSP/1.0\r\n"
        ));
        assert!(head.contains("CSeq: 9\r\n"));
        assert!(head.contains("Content-Type: application/x-dmap-tagged\r\n"));
        assert!(head.contains("RTP-Info: rtptime=270544960\r\n"));
        assert!(!head.contains("Client-Instance:"));
    }
}
