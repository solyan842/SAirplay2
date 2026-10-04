#![cfg(windows)]

//! In-process Windows RAOP adapter for the unified MSA Receiver Core.
//! Route/lifecycle semantics remain pinned to Music Assistant; only the old
//! cliraop helper/process boundary is removed.

use crate::timing::StartResolution;
use std::ffi::{c_char, c_int, c_void, CString};
use std::fmt;
use std::net::IpAddr;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MSA_LIBRAOP_PIN: &str = "81c2182649da8645ac2a58b78e9f370c79a4165b";
pub const RAOP_FRAMES_PER_PACKET: usize = 352;
pub const RAOP_PCM_PACKET_BYTES: usize = RAOP_FRAMES_PER_PACKET * 4;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

fn input_bytes_per_frame(bit_depth: u16, channels: u16) -> usize {
    (if bit_depth <= 16 { 2 } else { 4 }) * channels as usize
}

#[repr(C)]
struct SrRaopHandle {
    _opaque: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SrRaopReady {
    latency_frames: u32,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
}

unsafe extern "C" {
    fn sr_raop_open(
        host_name: *const c_char,
        port: u16,
        volume: u8,
        lead_ms: u32,
        sample_rate: u32,
        bit_depth: u16,
        channels: u16,
        compressed_alac: c_int,
        auth: c_int,
        encrypt: c_int,
        secret: *const c_char,
        password: *const c_char,
        et: *const c_char,
        md: *const c_char,
        dacp_id: *const c_char,
        active_remote: *const c_char,
        bind_ip: *const c_char,
        out_handle: *mut *mut SrRaopHandle,
        out_ready: *mut SrRaopReady,
        error: *mut c_char,
        error_cap: usize,
    ) -> c_int;
    fn sr_raop_close(handle: *mut SrRaopHandle);
    fn sr_raop_healthy(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_keepalive(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_commit_start(handle: *mut SrRaopHandle, requested_ms: u64, at_ms: *mut u64) -> c_int;
    fn sr_raop_start_after_flush(handle: *mut SrRaopHandle, requested_ms: u64, at_ms: *mut u64) -> c_int;
    fn sr_raop_flush(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_standby(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_pause(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_play(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_stop(handle: *mut SrRaopHandle) -> c_int;
    fn sr_raop_set_volume(handle: *mut SrRaopHandle, percent: u8) -> c_int;
    fn sr_raop_set_progress(handle: *mut SrRaopHandle, elapsed_s: u32, duration_s: u32) -> c_int;
    fn sr_raop_set_metadata(
        handle: *mut SrRaopHandle,
        title: *const c_char,
        artist: *const c_char,
        album: *const c_char,
    ) -> c_int;
    fn sr_raop_set_artwork(
        handle: *mut SrRaopHandle,
        content_type: *const c_char,
        data: *const u8,
        size: usize,
    ) -> c_int;
    fn sr_raop_write(handle: *mut SrRaopHandle, packet: *const u8, packet_bytes: usize) -> c_int;
    fn sr_raop_head_audible_ms(handle: *mut SrRaopHandle) -> u64;
}

#[derive(Debug, Clone)]
pub struct MsaRaopConfig {
    pub host: String,
    pub port: u16,
    pub volume: u8,
    pub et: String,
    pub md: String,
    pub secret: Option<String>,
    pub password: Option<String>,
    pub compressed_alac: bool,
    pub mfi_auth: bool,
    pub encrypt: bool,
    pub dacp_id: String,
    pub active_remote: String,
    pub bind_ip: Option<IpAddr>,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub lead_ms: u32,
}

impl MsaRaopConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            volume: 50,
            et: "0,4".into(),
            md: "0,1,2".into(),
            secret: None,
            password: None,
            compressed_alac: true,
            mfi_auth: false,
            encrypt: false,
            dacp_id: "1A2B3D4EA1B2C3D4".into(),
            active_remote: "0".into(),
            bind_ip: None,
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            lead_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaRaopState {
    Connected,
    Streaming,
    Flushed,
    Paused,
    Stopped,
    Down,
}

#[derive(Debug)]
pub enum MsaRaopError {
    InvalidInput(String),
    Connect(String),
    Operation(&'static str),
    Poisoned,
}

impl fmt::Display for MsaRaopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(v) => write!(f, "MSA RAOP invalid input: {v}"),
            Self::Connect(v) => write!(f, "MSA RAOP connect failed: {v}"),
            Self::Operation(v) => write!(f, "MSA RAOP in-process operation failed: {v}"),
            Self::Poisoned => write!(f, "MSA RAOP in-process transport mutex poisoned"),
        }
    }
}
impl std::error::Error for MsaRaopError {}

#[derive(Debug, Clone, Copy)]
pub struct MsaRaopReady {
    pub latency_frames: u32,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
}

struct RaopNative {
    handle: *mut SrRaopHandle,
}

// All calls into the pinned C transport are serialized by Mutex<RaopNative>.
unsafe impl Send for RaopNative {}

impl RaopNative {
    fn operation(&mut self, name: &'static str, f: impl FnOnce(*mut SrRaopHandle) -> c_int) -> Result<(), MsaRaopError> {
        if self.handle.is_null() || f(self.handle) == 0 {
            Err(MsaRaopError::Operation(name))
        } else {
            Ok(())
        }
    }

    fn close(&mut self) {
        if !self.handle.is_null() {
            unsafe { sr_raop_close(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}
impl Drop for RaopNative {
    fn drop(&mut self) { self.close(); }
}

#[derive(Clone)]
pub struct MsaRaopPcmWriter {
    inner: Arc<Mutex<RaopNative>>,
    packet_bytes: usize,
}

impl MsaRaopPcmWriter {
    pub fn write_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        if packet.len() != self.packet_bytes {
            return Err(MsaRaopError::InvalidInput(format!(
                "RAOP PCM packet must be {} bytes, got {}",
                self.packet_bytes,
                packet.len()
            )));
        }
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        native.operation("write", |handle| unsafe {
            sr_raop_write(handle, packet.as_ptr(), packet.len())
        })
    }
}

pub struct MsaRaopSession {
    inner: Arc<Mutex<RaopNative>>,
    meta_delivered: bool,
    meta_title: String,
    meta_artist: String,
    meta_album: String,
    meta_duration_s: u32,
    meta_item_id: String,
    state: MsaRaopState,
    ready: MsaRaopReady,
    log: Arc<Mutex<Vec<String>>>,
    last_keepalive: Instant,
}

fn cstring(label: &str, value: &str) -> Result<CString, MsaRaopError> {
    CString::new(value).map_err(|_| MsaRaopError::InvalidInput(format!("{label} contains NUL")))
}

fn optional_ptr(value: Option<&CString>) -> *const c_char {
    value.map_or(ptr::null(), |v| v.as_ptr())
}

impl MsaRaopSession {
    pub fn connect(config: MsaRaopConfig) -> Result<Self, MsaRaopError> {
        let host = cstring("host", &config.host)?;
        let et = cstring("et", &config.et)?;
        let md = cstring("md", &config.md)?;
        let dacp = cstring("dacp_id", &config.dacp_id)?;
        let active_remote = cstring("active_remote", &config.active_remote)?;
        let secret = config.secret.as_deref().map(|v| cstring("secret", v)).transpose()?;
        let password = config.password.as_deref().map(|v| cstring("password", v)).transpose()?;
        let bind = config.bind_ip.map(|v| cstring("bind_ip", &v.to_string())).transpose()?;

        let mut handle = ptr::null_mut();
        let mut ready = SrRaopReady::default();
        let mut error = vec![0i8; 512];
        let rc = unsafe {
            sr_raop_open(
                host.as_ptr(),
                config.port,
                config.volume.min(100),
                config.lead_ms,
                config.sample_rate,
                config.bit_depth,
                config.channels,
                config.compressed_alac as c_int,
                config.mfi_auth as c_int,
                config.encrypt as c_int,
                optional_ptr(secret.as_ref()),
                optional_ptr(password.as_ref()),
                et.as_ptr(),
                md.as_ptr(),
                dacp.as_ptr(),
                active_remote.as_ptr(),
                optional_ptr(bind.as_ref()),
                &mut handle,
                &mut ready,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if rc != 0 || handle.is_null() {
            let bytes = error.iter().map(|v| *v as u8).take_while(|v| *v != 0).collect::<Vec<_>>();
            let detail = String::from_utf8_lossy(&bytes).trim().to_owned();
            return Err(MsaRaopError::Connect(if detail.is_empty() {
                format!("native bridge status={rc}")
            } else {
                format!("native bridge status={rc}: {detail}")
            }));
        }

        let ready = MsaRaopReady {
            latency_frames: ready.latency_frames,
            sample_rate: ready.sample_rate,
            bit_depth: ready.bit_depth,
            channels: ready.channels,
        };
        let log = Arc::new(Mutex::new(vec![format!(
            "MSA-RAOP READY integration=in-process-static libraop={} latency={} sample_rate={} bit_depth={} channels={}",
            MSA_LIBRAOP_PIN, ready.latency_frames, ready.sample_rate, ready.bit_depth, ready.channels
        )]));
        Ok(Self {
            inner: Arc::new(Mutex::new(RaopNative { handle })),
            meta_delivered: false,
            meta_title: String::new(),
            meta_artist: String::new(),
            meta_album: String::new(),
            meta_duration_s: 0,
            meta_item_id: String::new(),
            state: MsaRaopState::Connected,
            ready,
            log,
            last_keepalive: Instant::now(),
        })
    }

    pub fn ready(&self) -> MsaRaopReady { self.ready }
    pub fn state(&self) -> MsaRaopState { self.state }
    pub fn logs(&self) -> Vec<String> { self.log.lock().map(|v| v.clone()).unwrap_or_default() }

    pub fn transport_healthy(&mut self) -> bool {
        let mut native = match self.inner.lock() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if native.handle.is_null() {
            return false;
        }
        if self.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            if unsafe { sr_raop_keepalive(native.handle) } == 0 {
                return false;
            }
            self.last_keepalive = Instant::now();
        }
        unsafe { sr_raop_healthy(native.handle) != 0 }
    }

    pub fn head_audible_unix_ms(&self) -> u64 {
        self.inner.lock().ok().map(|native| unsafe {
            if native.handle.is_null() { 0 } else { sr_raop_head_audible_ms(native.handle) }
        }).unwrap_or(0)
    }

    fn start_call(&mut self, requested_unix_ms: u64, after_flush: bool) -> Result<StartResolution, MsaRaopError> {
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        if native.handle.is_null() { return Err(MsaRaopError::Operation("start")); }
        let mut at = 0u64;
        let ok = unsafe {
            if after_flush {
                sr_raop_start_after_flush(native.handle, requested_unix_ms, &mut at)
            } else {
                sr_raop_commit_start(native.handle, requested_unix_ms, &mut at)
            }
        };
        if ok == 0 { return Err(MsaRaopError::Operation(if after_flush { "start_after_flush" } else { "start" })); }
        self.state = MsaRaopState::Streaming;
        Ok(StartResolution {
            requested_unix_ms,
            at_unix_ms: at,
            corrected_forward: requested_unix_ms != 0 && at != requested_unix_ms,
        })
    }

    pub fn commit_start(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {
        self.start_call(requested_unix_ms, false)
    }
    pub fn start_after_flush(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {
        self.start_call(requested_unix_ms, true)
    }

    fn simple(&mut self, name: &'static str, call: unsafe extern "C" fn(*mut SrRaopHandle) -> c_int, next: MsaRaopState) -> Result<(), MsaRaopError> {
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        native.operation(name, |handle| unsafe { call(handle) })?;
        self.state = next;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), MsaRaopError> { self.simple("flush", sr_raop_flush, MsaRaopState::Flushed) }
    pub fn standby(&mut self) -> Result<(), MsaRaopError> { self.simple("standby", sr_raop_standby, MsaRaopState::Connected) }
    pub fn pause(&mut self) -> Result<(), MsaRaopError> { self.simple("pause", sr_raop_pause, MsaRaopState::Paused) }
    pub fn play(&mut self) -> Result<(), MsaRaopError> { self.simple("play", sr_raop_play, MsaRaopState::Streaming) }
    pub fn stop(&mut self) -> Result<(), MsaRaopError> { self.simple("stop", sr_raop_stop, MsaRaopState::Stopped) }

    pub fn set_volume(&mut self, percent: u8) -> Result<(), MsaRaopError> {
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        native.operation("volume", |handle| unsafe { sr_raop_set_volume(handle, percent.min(100)) })
    }

    pub fn set_progress(&mut self, elapsed_s: u32, duration_s: u32) -> Result<(), MsaRaopError> {
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        native.operation("progress", |handle| unsafe { sr_raop_set_progress(handle, elapsed_s, duration_s) })
    }

    pub fn set_metadata(&mut self, title: &str, artist: &str, album: &str, duration_s: u32, item_id: &str) -> Result<(), MsaRaopError> {
        if self.meta_delivered && self.meta_title == title && self.meta_artist == artist
            && self.meta_album == album && self.meta_duration_s == duration_s && self.meta_item_id == item_id {
            return Ok(());
        }
        let title_c = cstring("title", title)?;
        let artist_c = cstring("artist", artist)?;
        let album_c = cstring("album", album)?;
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        native.operation("metadata", |handle| unsafe {
            sr_raop_set_metadata(handle, title_c.as_ptr(), artist_c.as_ptr(), album_c.as_ptr())
        })?;
        self.meta_delivered = true;
        self.meta_title = title.to_owned();
        self.meta_artist = artist.to_owned();
        self.meta_album = album.to_owned();
        self.meta_duration_s = duration_s;
        self.meta_item_id = item_id.to_owned();
        Ok(())
    }

    pub fn ensure_initial_metadata(&mut self) -> Result<(), MsaRaopError> {
        if self.meta_delivered { Ok(()) } else { self.set_metadata("cliairplay", "", "", 0, "") }
    }

    pub fn set_artwork(&mut self, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
        let mime = cstring("content_type", content_type)?;
        let mut native = self.inner.lock().map_err(|_| MsaRaopError::Poisoned)?;
        native.operation("artwork", |handle| unsafe {
            sr_raop_set_artwork(handle, mime.as_ptr(), data.as_ptr(), data.len())
        })
    }

    pub fn pcm_writer(&self) -> MsaRaopPcmWriter {
        MsaRaopPcmWriter {
            inner: Arc::clone(&self.inner),
            packet_bytes: RAOP_FRAMES_PER_PACKET * input_bytes_per_frame(self.ready.bit_depth, self.ready.channels),
        }
    }

    pub fn write_pcm_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        self.pcm_writer().write_packet(packet)
    }

    pub fn disconnect(&mut self) {
        if let Ok(mut native) = self.inner.lock() { native.close(); }
        self.state = MsaRaopState::Down;
    }
}

impl Drop for MsaRaopSession {
    fn drop(&mut self) { self.disconnect(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_pin_is_exact_msa_submodule_pin() {
        assert_eq!(MSA_LIBRAOP_PIN, "81c2182649da8645ac2a58b78e9f370c79a4165b");
    }

    #[test]
    fn packet_contract_tracks_requested_format() {
        assert_eq!(RAOP_FRAMES_PER_PACKET * input_bytes_per_frame(16, 2), 1408);
        assert_eq!(RAOP_FRAMES_PER_PACKET * input_bytes_per_frame(24, 2), 2816);
    }
}
