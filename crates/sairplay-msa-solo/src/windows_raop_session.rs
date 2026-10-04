#![cfg(windows)]

//! Concrete Windows RAOP transport for the independent MSA SOLO engine.
//! Prefer the in-process bridge when a matching x64 `sairplay-raop.dll` is
//! present; retain the #1397 helper-process backend as an explicit rollback
//! path until the new transport is hardware-validated.

use crate::timing::StartResolution;
use libloading::Library;
use sairplay_helper_process::ManagedChild;
use std::ffi::{c_char, c_void, CString};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
    Arc, Mutex, RwLock,
};
use std::thread;
use std::time::{Duration, Instant};

pub const MSA_LIBRAOP_PIN: &str = "81c2182649da8645ac2a58b78e9f370c79a4165b";
pub const RAOP_FRAMES_PER_PACKET: usize = 352;
pub const RAOP_PCM_PACKET_BYTES: usize = RAOP_FRAMES_PER_PACKET * 4;

fn input_bytes_per_frame(bit_depth: u16, channels: u16) -> usize {
    (if bit_depth <= 16 { 2 } else { 4 }) * channels as usize
}
const COMMAND_TIMEOUT: Duration = Duration::from_secs(12);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const DISCONNECT_GRACE: Duration = Duration::from_secs(2);
const CREATE_NO_WINDOW: u32 = 0x08000000;
const INPROC_KEEPALIVE: Duration = Duration::from_secs(20);
static SESSION_ID: AtomicU64 = AtomicU64::new(1);

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
pub enum MsaRaopState { Connected, Streaming, Flushed, Paused, Stopped, Down }

#[derive(Debug)]
pub enum MsaRaopError {
    HelperMissing(PathBuf),
    Spawn(std::io::Error),
    Pipe,
    ReadinessTimeout,
    Connect(String),
    Io(std::io::Error),
    AckTimeout { seq: u64 },
    Command { seq: u64, command: String, detail: String },
    InvalidAck(String),
    InProcess(String),
}
impl fmt::Display for MsaRaopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelperMissing(p) => write!(f, "MSA RAOP helper missing: {}", p.display()),
            Self::Spawn(e) => write!(f, "MSA RAOP helper spawn failed: {e}"),
            Self::Pipe => write!(f, "MSA RAOP helper pipe missing"),
            Self::ReadinessTimeout => write!(f, "MSA RAOP helper readiness timed out"),
            Self::Connect(s) => write!(f, "MSA RAOP connect failed: {s}"),
            Self::Io(e) => write!(f, "MSA RAOP I/O failed: {e}"),
            Self::AckTimeout { seq } => write!(f, "MSA RAOP command {seq} timed out"),
            Self::Command { seq, command, detail } =>
                write!(f, "MSA RAOP command {seq} {command} failed: {detail}"),
            Self::InvalidAck(s) => write!(f, "invalid MSA RAOP ack: {s}"),
            Self::InProcess(s) => write!(f, "MSA RAOP in-process backend failed: {s}"),
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

#[repr(C)]
struct SrRaopConfig {
    host: *const c_char,
    bind_ip: *const c_char,
    port: u16,
    volume: u8,
    et: *const c_char,
    md: *const c_char,
    secret: *const c_char,
    password: *const c_char,
    compressed_alac: i32,
    mfi_auth: i32,
    encrypt: i32,
    dacp_id: *const c_char,
    active_remote: *const c_char,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
    lead_ms: u32,
}

#[repr(C)]
#[derive(Default)]
struct SrRaopReady {
    latency_frames: u32,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
}

type OpenFn = unsafe extern "C" fn(*const SrRaopConfig, *mut SrRaopReady) -> *mut c_void;
type CloseFn = unsafe extern "C" fn(*mut c_void);
type BoolFn = unsafe extern "C" fn(*mut c_void) -> i32;
type StartFn = unsafe extern "C" fn(*mut c_void, u64, *mut u64) -> i32;
type VolumeFn = unsafe extern "C" fn(*mut c_void, u8) -> i32;
type ProgressFn = unsafe extern "C" fn(*mut c_void, u32, u32) -> i32;
type MetadataFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char) -> i32;
type ArtworkFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize) -> i32;
type WriteFn = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;
type HeadFn = unsafe extern "C" fn(*mut c_void) -> u64;

struct InprocApi {
    _library: Library,
    open: OpenFn,
    close: CloseFn,
    healthy: BoolFn,
    keepalive: BoolFn,
    commit_start: StartFn,
    start_after_flush: StartFn,
    flush: BoolFn,
    standby: BoolFn,
    pause: BoolFn,
    play: BoolFn,
    stop: BoolFn,
    set_volume: VolumeFn,
    set_progress: ProgressFn,
    set_metadata: MetadataFn,
    set_artwork: ArtworkFn,
    write_packet: WriteFn,
    head_audible_ms: HeadFn,
}

impl InprocApi {
    unsafe fn load(path: &Path) -> Result<Self, MsaRaopError> {
        let library = Library::new(path)
            .map_err(|e| MsaRaopError::InProcess(format!("load {}: {e}", path.display())))?;
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                *library.get::<$ty>(concat!($name, "\0").as_bytes())
                    .map_err(|e| MsaRaopError::InProcess(format!("symbol {}: {e}", $name)))?
            }};
        }
        let open = symbol!("sr_raop_open", OpenFn);
        let close = symbol!("sr_raop_close", CloseFn);
        let healthy = symbol!("sr_raop_healthy", BoolFn);
        let keepalive = symbol!("sr_raop_keepalive", BoolFn);
        let commit_start = symbol!("sr_raop_commit_start", StartFn);
        let start_after_flush = symbol!("sr_raop_start_after_flush", StartFn);
        let flush = symbol!("sr_raop_flush", BoolFn);
        let standby = symbol!("sr_raop_standby", BoolFn);
        let pause = symbol!("sr_raop_pause", BoolFn);
        let play = symbol!("sr_raop_play", BoolFn);
        let stop = symbol!("sr_raop_stop", BoolFn);
        let set_volume = symbol!("sr_raop_set_volume", VolumeFn);
        let set_progress = symbol!("sr_raop_set_progress", ProgressFn);
        let set_metadata = symbol!("sr_raop_set_metadata", MetadataFn);
        let set_artwork = symbol!("sr_raop_set_artwork", ArtworkFn);
        let write_packet = symbol!("sr_raop_write_packet", WriteFn);
        let head_audible_ms = symbol!("sr_raop_head_audible_ms", HeadFn);
        Ok(Self {
            _library: library, open, close, healthy, keepalive, commit_start,
            start_after_flush, flush, standby, pause, play, stop, set_volume,
            set_progress, set_metadata, set_artwork, write_packet, head_audible_ms,
        })
    }
}

struct InprocStrings {
    _host: CString,
    _bind_ip: Option<CString>,
    _et: CString,
    _md: CString,
    _secret: CString,
    _password: Option<CString>,
    _dacp_id: CString,
    _active_remote: CString,
}

struct InprocCore {
    api: InprocApi,
    handle: RwLock<Option<usize>>,
    last_keepalive: Mutex<Instant>,
    _strings: InprocStrings,
}

impl InprocCore {
    fn with_handle<T>(&self, f: impl FnOnce(*mut c_void) -> T) -> Result<T, MsaRaopError> {
        let guard = self.handle.read().map_err(|_| MsaRaopError::InProcess("handle lock poisoned".into()))?;
        let raw = guard.ok_or_else(|| MsaRaopError::InProcess("session already closed".into()))?;
        Ok(f(raw as *mut c_void))
    }

    fn close(&self) {
        if let Ok(mut guard) = self.handle.write() {
            if let Some(raw) = guard.take() {
                unsafe { (self.api.close)(raw as *mut c_void) };
            }
        }
    }

    fn healthy(&self) -> Result<bool, MsaRaopError> {
        let ok = self.with_handle(|p| unsafe { (self.api.healthy)(p) != 0 })?;
        if !ok { return Ok(false); }
        let mut last = self.last_keepalive.lock()
            .map_err(|_| MsaRaopError::InProcess("keepalive lock poisoned".into()))?;
        if last.elapsed() >= INPROC_KEEPALIVE {
            let keepalive_ok = self.with_handle(|p| unsafe { (self.api.keepalive)(p) != 0 })?;
            if !keepalive_ok { return Ok(false); }
            *last = Instant::now();
        }
        Ok(true)
    }

    fn start(&self, requested_unix_ms: u64, after_flush: bool) -> Result<u64, MsaRaopError> {
        let mut at = 0u64;
        let ok = self.with_handle(|p| unsafe {
            if after_flush {
                (self.api.start_after_flush)(p, requested_unix_ms, &mut at)
            } else {
                (self.api.commit_start)(p, requested_unix_ms, &mut at)
            }
        })?;
        if ok != 0 { Ok(at) } else { Err(MsaRaopError::InProcess("START rejected".into())) }
    }

    fn simple(&self, f: BoolFn, name: &str) -> Result<(), MsaRaopError> {
        let ok = self.with_handle(|p| unsafe { f(p) })?;
        if ok != 0 { Ok(()) } else { Err(MsaRaopError::InProcess(format!("{name} rejected"))) }
    }

    fn set_volume(&self, percent: u8) -> Result<(), MsaRaopError> {
        let ok = self.with_handle(|p| unsafe { (self.api.set_volume)(p, percent.min(100)) })?;
        if ok != 0 { Ok(()) } else { Err(MsaRaopError::InProcess("VOLUME rejected".into())) }
    }

    fn set_progress(&self, elapsed_s: u32, duration_s: u32) -> Result<(), MsaRaopError> {
        let ok = self.with_handle(|p| unsafe { (self.api.set_progress)(p, elapsed_s, duration_s) })?;
        if ok != 0 { Ok(()) } else { Err(MsaRaopError::InProcess("PROGRESS rejected".into())) }
    }

    fn set_metadata(&self, title: &str, artist: &str, album: &str) -> Result<(), MsaRaopError> {
        let title = cstring(title, "title")?;
        let artist = cstring(artist, "artist")?;
        let album = cstring(album, "album")?;
        let ok = self.with_handle(|p| unsafe {
            (self.api.set_metadata)(p, title.as_ptr(), artist.as_ptr(), album.as_ptr())
        })?;
        if ok != 0 { Ok(()) } else { Err(MsaRaopError::InProcess("METADATA rejected".into())) }
    }

    fn set_artwork(&self, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
        let content_type = cstring(content_type, "artwork content type")?;
        let ok = self.with_handle(|p| unsafe {
            (self.api.set_artwork)(p, content_type.as_ptr(), data.as_ptr(), data.len())
        })?;
        if ok != 0 { Ok(()) } else { Err(MsaRaopError::InProcess("ARTWORK rejected".into())) }
    }

    fn write_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        let ok = self.with_handle(|p| unsafe {
            (self.api.write_packet)(p, packet.as_ptr(), packet.len())
        })?;
        if ok != 0 { Ok(()) } else { Err(MsaRaopError::InProcess("PCM write rejected".into())) }
    }

    fn head_audible_ms(&self) -> u64 {
        self.with_handle(|p| unsafe { (self.api.head_audible_ms)(p) }).unwrap_or(0)
    }
}
impl Drop for InprocCore { fn drop(&mut self) { self.close(); } }

fn cstring(value: &str, field: &str) -> Result<CString, MsaRaopError> {
    CString::new(value).map_err(|_| MsaRaopError::InProcess(format!("{field} contains NUL")))
}

fn inproc_dll_path() -> Result<Option<PathBuf>, MsaRaopError> {
    if let Some(path) = std::env::var_os("SAIRPLAY_RAOP_INPROC_DLL") {
        let path = PathBuf::from(path);
        if path.is_file() { return Ok(Some(path)); }
        return Err(MsaRaopError::InProcess(format!("configured DLL missing: {}", path.display())));
    }
    let exe = std::env::current_exe().map_err(MsaRaopError::Io)?;
    let path = exe.parent().unwrap_or(Path::new(".")).join("sairplay-raop.dll");
    Ok(path.is_file().then_some(path))
}

fn try_open_inproc(config: &MsaRaopConfig) -> Result<Option<(Arc<InprocCore>, MsaRaopReady, PathBuf)>, MsaRaopError> {
    let Some(path) = inproc_dll_path()? else { return Ok(None) };
    let api = unsafe { InprocApi::load(&path)? };

    let host = cstring(&config.host, "host")?;
    let bind_ip = config.bind_ip.map(|v| cstring(&v.to_string(), "bind_ip")).transpose()?;
    let et = cstring(&config.et, "et")?;
    let md = cstring(&config.md, "md")?;
    let secret = cstring(config.secret.as_deref().unwrap_or(""), "secret")?;
    let password = config.password.as_deref().map(|v| cstring(v, "password")).transpose()?;
    let dacp_id = cstring(&config.dacp_id, "dacp_id")?;
    let active_remote = cstring(&config.active_remote, "active_remote")?;

    let ffi = SrRaopConfig {
        host: host.as_ptr(),
        bind_ip: bind_ip.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
        port: config.port,
        volume: config.volume.min(100),
        et: et.as_ptr(),
        md: md.as_ptr(),
        secret: secret.as_ptr(),
        password: password.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
        compressed_alac: i32::from(config.compressed_alac),
        mfi_auth: i32::from(config.mfi_auth),
        encrypt: i32::from(config.encrypt),
        dacp_id: dacp_id.as_ptr(),
        active_remote: active_remote.as_ptr(),
        sample_rate: config.sample_rate,
        bit_depth: config.bit_depth,
        channels: config.channels,
        lead_ms: config.lead_ms,
    };
    let mut ready = SrRaopReady::default();
    let raw = unsafe { (api.open)(&ffi, &mut ready) };
    if raw.is_null() {
        return Err(MsaRaopError::InProcess("sr_raop_open returned null".into()));
    }
    let core = Arc::new(InprocCore {
        api,
        handle: RwLock::new(Some(raw as usize)),
        last_keepalive: Mutex::new(Instant::now()),
        _strings: InprocStrings {
            _host: host,
            _bind_ip: bind_ip,
            _et: et,
            _md: md,
            _secret: secret,
            _password: password,
            _dacp_id: dacp_id,
            _active_remote: active_remote,
        },
    });
    Ok(Some((core, MsaRaopReady {
        latency_frames: ready.latency_frames,
        sample_rate: ready.sample_rate,
        bit_depth: ready.bit_depth,
        channels: ready.channels,
    }, path)))
}

#[derive(Clone)]
enum PcmWriterBackend {
    Helper(Arc<Mutex<ChildStdin>>),
    InProcess(Arc<InprocCore>),
}

#[derive(Clone)]
pub struct MsaRaopPcmWriter {
    backend: PcmWriterBackend,
    packet_bytes: usize,
}
impl MsaRaopPcmWriter {
    pub fn write_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        if packet.len() != self.packet_bytes {
            return Err(MsaRaopError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("RAOP PCM packet must be {} bytes", self.packet_bytes),
            )));
        }
        match &self.backend {
            PcmWriterBackend::Helper(stdin) => stdin.lock().map_err(|_| MsaRaopError::Pipe)?
                .write_all(packet).map_err(MsaRaopError::Io),
            PcmWriterBackend::InProcess(core) => core.write_packet(packet),
        }
    }
}

pub struct MsaRaopSession {
    inproc: Option<Arc<InprocCore>>,
    child: Option<ManagedChild>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    control_path: Option<PathBuf>,
    ack_path: Option<PathBuf>,
    metadata_path: Option<PathBuf>,
    artwork_path: Option<PathBuf>,
    head_audible_ms: Arc<AtomicU64>,
    meta_delivered: bool,
    meta_title: String,
    meta_artist: String,
    meta_album: String,
    meta_duration_s: u32,
    meta_item_id: String,
    next_seq: u64,
    state: MsaRaopState,
    ready: MsaRaopReady,
    log: Arc<Mutex<Vec<String>>>,
}

impl MsaRaopSession {
    pub fn connect(config: MsaRaopConfig) -> Result<Self, MsaRaopError> {
        if let Some((core, ready, path)) = try_open_inproc(&config)? {
            return Ok(Self {
                inproc: Some(core),
                child: None,
                stdin: None,
                control_path: None,
                ack_path: None,
                metadata_path: None,
                artwork_path: None,
                head_audible_ms: Arc::new(AtomicU64::new(0)),
                meta_delivered: false,
                meta_title: String::new(),
                meta_artist: String::new(),
                meta_album: String::new(),
                meta_duration_s: 0,
                meta_item_id: String::new(),
                next_seq: 1,
                state: MsaRaopState::Connected,
                ready,
                log: Arc::new(Mutex::new(vec![format!(
                    "MSA-RAOP backend=in-process dll={}", path.display()
                )])),
            });
        }

        let helper = helper_path()?;
        let id = SESSION_ID.fetch_add(1, Ordering::SeqCst);
        let stem = format!("sairplay-msa-raop-{}-{id}", std::process::id());
        let control_path = std::env::temp_dir().join(format!("{stem}.cmd"));
        let ack_path = std::env::temp_dir().join(format!("{stem}.ack"));
        let metadata_path = std::env::temp_dir().join(format!("{stem}.meta"));
        let artwork_path = std::env::temp_dir().join(format!("{stem}.art"));
        let _ = std::fs::remove_file(&control_path);
        let _ = std::fs::remove_file(&ack_path);
        let _ = std::fs::remove_file(&metadata_path);
        let _ = std::fs::remove_file(&artwork_path);

        let mut cmd = Command::new(&helper);
        cmd.arg("--control").arg(&control_path)
            .arg("--ack").arg(&ack_path)
            .arg("--metadata").arg(&metadata_path)
            .arg("--artwork").arg(&artwork_path)
            .arg("-p").arg(config.port.to_string())
            .arg("-v").arg(config.volume.min(100).to_string())
            .arg("-l").arg(config.lead_ms.to_string())
            .arg("-r").arg(config.sample_rate.to_string())
            .arg("-b").arg(config.bit_depth.to_string())
            .arg("-c").arg(config.channels.to_string())
            .arg("-D").arg(&config.dacp_id)
            .arg("-R").arg(&config.active_remote)
            .arg("-t").arg(&config.et)
            .arg("-m").arg(&config.md);
        if let Some(bind_ip) = config.bind_ip { cmd.arg("--bind").arg(bind_ip.to_string()); }
        if config.encrypt { cmd.arg("-e"); }
        if !config.compressed_alac { cmd.arg("--pcm"); }
        if config.mfi_auth { cmd.arg("-u"); }
        if let Some(secret) = config.secret.as_deref().filter(|v| !v.trim().is_empty()) { cmd.arg("-s").arg(secret); }
        if let Some(password) = config.password.as_deref().filter(|v| !v.is_empty()) { cmd.arg("-P").arg(password); }
        cmd.arg(&config.host)
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);

        let mut child = ManagedChild::spawn(&mut cmd).map_err(MsaRaopError::Spawn)?;
        let stdin = Arc::new(Mutex::new(child.take_stdin().ok_or(MsaRaopError::Pipe)?));
        let stderr = child.take_stderr().ok_or(MsaRaopError::Pipe)?;
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<MsaRaopReady, String>>(1);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let head_audible_ms = Arc::new(AtomicU64::new(0));
        let log_t = Arc::clone(&log);
        let head_t = Arc::clone(&head_audible_ms);
        thread::Builder::new().name("msa-raop-log".into()).spawn(move || {
            let mut reported = false;
            let mut last_error = None::<String>;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut lines) = log_t.lock() {
                    if lines.len() >= 128 { lines.remove(0); }
                    lines.push(line.clone());
                }
                if let Some(rest) = line.strip_prefix("MSA-RAOP READY ") {
                    let mut latency = None; let mut rate = None; let mut depth = None; let mut channels = None;
                    for token in rest.split_whitespace() {
                        if let Some(v) = token.strip_prefix("latency=") { latency = v.parse::<u32>().ok(); }
                        else if let Some(v) = token.strip_prefix("sample_rate=") { rate = v.parse::<u32>().ok(); }
                        else if let Some(v) = token.strip_prefix("bit_depth=") { depth = v.parse::<u16>().ok(); }
                        else if let Some(v) = token.strip_prefix("channels=") { channels = v.parse::<u16>().ok(); }
                    }
                    if let (Some(latency_frames), Some(sample_rate), Some(bit_depth), Some(channels)) = (latency, rate, depth, channels) {
                        let _ = ready_tx.send(Ok(MsaRaopReady { latency_frames, sample_rate, bit_depth, channels }));
                        reported = true;
                    }
                } else if let Some(rest) = line.strip_prefix("MSA-RAOP HEAD ") {
                    for token in rest.split_whitespace() {
                        if let Some(v) = token.strip_prefix("audible_ms=") {
                            if let Ok(ms) = v.parse::<u64>() { head_t.store(ms, Ordering::SeqCst); }
                        }
                    }
                } else if line.starts_with("MSA-RAOP ERROR ") { last_error = Some(line); }
            }
            if !reported {
                let _ = ready_tx.send(Err(last_error.unwrap_or_else(|| "helper exited before reporting readiness".into())));
            }
        }).map_err(MsaRaopError::Spawn)?;

        let ready = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(MsaRaopError::Connect(e)),
            Err(_) => return Err(MsaRaopError::ReadinessTimeout),
        };

        Ok(Self {
            inproc: None,
            child: Some(child),
            stdin: Some(stdin),
            control_path: Some(control_path),
            ack_path: Some(ack_path),
            metadata_path: Some(metadata_path),
            artwork_path: Some(artwork_path),
            head_audible_ms,
            meta_delivered: false,
            meta_title: String::new(),
            meta_artist: String::new(),
            meta_album: String::new(),
            meta_duration_s: 0,
            meta_item_id: String::new(),
            next_seq: 1,
            state: MsaRaopState::Connected,
            ready,
            log,
        })
    }

    pub fn ready(&self) -> MsaRaopReady { self.ready }
    pub fn helper_alive(&mut self) -> bool {
        if let Some(core) = self.inproc.as_ref() { return core.healthy().unwrap_or(false); }
        self.child.as_mut().map(|child| matches!(child.try_wait(), Ok(None))).unwrap_or(false)
    }
    pub fn state(&self) -> MsaRaopState { self.state }
    pub fn logs(&self) -> Vec<String> { self.log.lock().map(|v| v.clone()).unwrap_or_default() }
    pub fn head_audible_unix_ms(&self) -> u64 {
        if let Some(core) = self.inproc.as_ref() { core.head_audible_ms() }
        else { self.head_audible_ms.load(Ordering::SeqCst) }
    }

    pub fn commit_start(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        let at = if let Some(core) = self.inproc.as_ref() { core.start(requested_unix_ms, false)? }
            else { self.command("START", requested_unix_ms, 0)? };
        self.state = MsaRaopState::Streaming;
        Ok(StartResolution { requested_unix_ms, at_unix_ms: at, corrected_forward: requested_unix_ms != 0 && at != requested_unix_ms })
    }
    pub fn start_after_flush(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        let at = if let Some(core) = self.inproc.as_ref() { core.start(requested_unix_ms, true)? }
            else { self.command("START_AFTER_FLUSH", requested_unix_ms, 0)? };
        self.state = MsaRaopState::Streaming;
        Ok(StartResolution { requested_unix_ms, at_unix_ms: at, corrected_forward: requested_unix_ms != 0 && at != requested_unix_ms })
    }
    pub fn flush(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        if let Some(core) = self.inproc.as_ref() { core.simple(core.api.flush, "FLUSH")?; }
        else { self.command("FLUSH", 0, 0)?; }
        self.state = MsaRaopState::Flushed; Ok(())
    }
    pub fn standby(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        if let Some(core) = self.inproc.as_ref() { core.simple(core.api.standby, "STANDBY")?; }
        else { self.command("STANDBY", 0, 0)?; }
        self.state = MsaRaopState::Connected; Ok(())
    }
    pub fn pause(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        if let Some(core) = self.inproc.as_ref() { core.simple(core.api.pause, "PAUSE")?; }
        else { self.command("PAUSE", 0, 0)?; }
        self.state = MsaRaopState::Paused; Ok(())
    }
    pub fn play(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        if let Some(core) = self.inproc.as_ref() { core.simple(core.api.play, "PLAY")?; }
        else { self.command("PLAY", 0, 0)?; }
        self.state = MsaRaopState::Streaming; Ok(())
    }
    pub fn stop(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        if let Some(core) = self.inproc.as_ref() { core.simple(core.api.stop, "STOP")?; }
        else { self.command("STOP", 0, 0)?; }
        self.state = MsaRaopState::Down; Ok(())
    }
    pub fn set_volume(&mut self, percent: u8) -> Result<(), MsaRaopError> {
        if let Some(core) = self.inproc.as_ref() { core.set_volume(percent) }
        else { self.command("VOLUME", percent.min(100) as u64, 0).map(|_| ()) }
    }
    pub fn set_progress(&mut self, elapsed_s: u32, duration_s: u32) -> Result<(), MsaRaopError> {
        if let Some(core) = self.inproc.as_ref() { core.set_progress(elapsed_s, duration_s) }
        else { self.command("PROGRESS", elapsed_s as u64, duration_s as u64).map(|_| ()) }
    }

    pub fn set_metadata(&mut self, title: &str, artist: &str, album: &str, duration_s: u32, item_id: &str) -> Result<(), MsaRaopError> {
        if self.meta_delivered && self.meta_title == title && self.meta_artist == artist && self.meta_album == album
            && self.meta_duration_s == duration_s && self.meta_item_id == item_id { return Ok(()); }
        if let Some(core) = self.inproc.as_ref() { core.set_metadata(title, artist, album)?; }
        else {
            let path = self.metadata_path.as_ref().ok_or(MsaRaopError::Pipe)?;
            write_metadata_sidecar(path, title, artist, album)?;
            self.command("METADATA", 0, 0)?;
        }
        self.meta_delivered = true;
        self.meta_title = title.to_owned(); self.meta_artist = artist.to_owned(); self.meta_album = album.to_owned();
        self.meta_duration_s = duration_s; self.meta_item_id = item_id.to_owned();
        Ok(())
    }
    pub fn ensure_initial_metadata(&mut self) -> Result<(), MsaRaopError> {
        if self.meta_delivered { Ok(()) } else { self.set_metadata("cliairplay", "", "", 0, "") }
    }
    pub fn set_artwork(&mut self, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
        if let Some(core) = self.inproc.as_ref() { core.set_artwork(content_type, data) }
        else {
            let path = self.artwork_path.as_ref().ok_or(MsaRaopError::Pipe)?;
            write_artwork_sidecar(path, content_type, data)?;
            self.command("ARTWORK", 0, 0).map(|_| ())
        }
    }

    pub fn pcm_writer(&self) -> MsaRaopPcmWriter {
        let backend = if let Some(core) = self.inproc.as_ref() {
            PcmWriterBackend::InProcess(Arc::clone(core))
        } else {
            PcmWriterBackend::Helper(Arc::clone(self.stdin.as_ref().expect("helper stdin missing")))
        };
        MsaRaopPcmWriter {
            backend,
            packet_bytes: RAOP_FRAMES_PER_PACKET * input_bytes_per_frame(self.ready.bit_depth, self.ready.channels),
        }
    }
    pub fn write_pcm_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> { self.pcm_writer().write_packet(packet) }

    fn enqueue_command(&mut self, name: &str, arg1: u64, arg2: u64) -> Result<u64, MsaRaopError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        let control_path = self.control_path.as_ref().ok_or(MsaRaopError::Pipe)?;
        let tmp = control_path.with_extension("cmd.tmp");
        std::fs::write(&tmp, format!("{seq} {name} {arg1} {arg2}\n")).map_err(MsaRaopError::Io)?;
        if control_path.exists() { let _ = std::fs::remove_file(control_path); }
        std::fs::rename(&tmp, control_path).map_err(MsaRaopError::Io)?;
        Ok(seq)
    }
    fn command(&mut self, name: &str, arg1: u64, arg2: u64) -> Result<u64, MsaRaopError> {
        let seq = self.enqueue_command(name, arg1, arg2)?;
        let ack_path = self.ack_path.as_ref().ok_or(MsaRaopError::Pipe)?.clone();
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            if let Ok(text) = std::fs::read_to_string(&ack_path) {
                if let Some((ack_seq, ok, at, detail)) = parse_ack(&text) {
                    if ack_seq == seq {
                        return if ok { Ok(at) } else { Err(MsaRaopError::Command { seq, command: name.into(), detail: detail.into() }) };
                    }
                }
            }
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(MsaRaopError::Connect(format!("helper exited while waiting for {name}: {status}")));
                }
            }
            if Instant::now() >= deadline { return Err(MsaRaopError::AckTimeout { seq }); }
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn disconnect(&mut self) {
        if let Some(core) = self.inproc.take() {
            core.close();
            self.state = MsaRaopState::Down;
            return;
        }
        if self.child.is_some() {
            let _ = self.enqueue_command("QUIT", 0, 0);
            if let Some(child) = self.child.as_mut() { let _ = child.wait_or_terminate(DISCONNECT_GRACE); }
        }
        self.state = MsaRaopState::Down;
        for path in [&self.control_path, &self.ack_path, &self.metadata_path, &self.artwork_path] {
            if let Some(path) = path.as_ref() { let _ = std::fs::remove_file(path); }
        }
    }
}
impl Drop for MsaRaopSession { fn drop(&mut self) { self.disconnect(); } }

fn write_u32_le(out: &mut Vec<u8>, value: usize) -> Result<(), MsaRaopError> {
    let value = u32::try_from(value).map_err(|_| MsaRaopError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput, "sidecar field too large")))?;
    out.extend_from_slice(&value.to_le_bytes()); Ok(())
}
fn write_metadata_sidecar(path: &Path, title: &str, artist: &str, album: &str) -> Result<(), MsaRaopError> {
    let mut out = Vec::new();
    for value in [title.as_bytes(), artist.as_bytes(), album.as_bytes()] {
        write_u32_le(&mut out, value.len())?; out.extend_from_slice(value);
    }
    std::fs::write(path, out).map_err(MsaRaopError::Io)
}
fn write_artwork_sidecar(path: &Path, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
    let mut out = Vec::new();
    write_u32_le(&mut out, content_type.len())?; out.extend_from_slice(content_type.as_bytes());
    write_u32_le(&mut out, data.len())?; out.extend_from_slice(data);
    std::fs::write(path, out).map_err(MsaRaopError::Io)
}
fn parse_ack(line: &str) -> Option<(u64, bool, u64, &str)> {
    let mut parts = line.trim().splitn(4, ' ');
    let seq = parts.next()?.parse().ok()?;
    let ok = match parts.next()? { "OK" => true, "ERR" => false, _ => return None };
    let at = parts.next()?.parse().ok()?;
    let detail = parts.next().unwrap_or("");
    Some((seq, ok, at, detail))
}
fn helper_path() -> Result<PathBuf, MsaRaopError> {
    if let Some(path) = std::env::var_os("SAIRPLAY_MSA_RAOP_HELPER") {
        let path = PathBuf::from(path);
        if path.is_file() { return Ok(path); }
        return Err(MsaRaopError::HelperMissing(path));
    }
    let exe = std::env::current_exe().map_err(MsaRaopError::Io)?;
    let path = exe.parent().unwrap_or(Path::new(".")).join("cliraop-msa-solo.exe");
    if path.is_file() { Ok(path) } else { Err(MsaRaopError::HelperMissing(path)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn ack_parser_preserves_scheduled_instant() {
        assert_eq!(parse_ack("8 OK 12345 START\n"), Some((8, true, 12345, "START")));
    }
    #[test] fn helper_pin_is_exact_msa_submodule_pin() {
        assert_eq!(MSA_LIBRAOP_PIN, "81c2182649da8645ac2a58b78e9f370c79a4165b");
    }
    #[test] fn default_inproc_name_is_not_the_helper_name() {
        let path = Path::new("sairplay-raop.dll");
        assert_ne!(path.file_name().unwrap(), "cliraop-msa-solo.exe");
    }
}
