#![cfg(windows)]

//! Concrete Windows RAOP transport for the independent MSA SOLO engine.
//! Owns a separate helper built from the exact libraop pin used by pinned MSA.
//! The helper process and raopcl_s persist across FLUSH/START; legacy engine
//! helpers are not touched or reused.

use crate::timing::StartResolution;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
    Arc, Mutex,
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
static SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Owns one Windows Job Object with KILL_ON_JOB_CLOSE. The raw handle is stored
/// as usize so the session stays Send when moved between GUI/worker threads.
/// Closing SAirplay2 (even while a connect thread is still blocked) closes the
/// job handle at process teardown, and Windows terminates the helper instead of
/// leaving cliraop-msa-solo.exe orphaned and locking the install directory.
struct KillOnCloseJob {
    handle: usize,
}

impl KillOnCloseJob {
    fn attach(child: &Child) -> Result<Self, MsaRaopError> {
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| MsaRaopError::Job(e.to_string()))?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set_result = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if let Err(error) = set_result {
            let _ = unsafe { CloseHandle(job) };
            return Err(MsaRaopError::Job(error.to_string()));
        }

        let process = HANDLE(child.as_raw_handle());
        if let Err(error) = unsafe { AssignProcessToJobObject(job, process) } {
            let _ = unsafe { CloseHandle(job) };
            return Err(MsaRaopError::Job(error.to_string()));
        }

        Ok(Self { handle: job.0 as usize })
    }

    fn terminate(&self) {
        let handle = HANDLE(self.handle as *mut core::ffi::c_void);
        let _ = unsafe { TerminateJobObject(handle, 1) };
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        let handle = HANDLE(self.handle as *mut core::ffi::c_void);
        let _ = unsafe { CloseHandle(handle) };
    }
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
pub enum MsaRaopState { Connected, Streaming, Flushed, Paused, Stopped, Down }

#[derive(Debug)]
pub enum MsaRaopError {
    HelperMissing(PathBuf),
    Spawn(std::io::Error),
    Job(String),
    Pipe,
    ReadinessTimeout,
    Connect(String),
    Io(std::io::Error),
    AckTimeout { seq: u64 },
    Command { seq: u64, command: String, detail: String },
    InvalidAck(String),
}
impl fmt::Display for MsaRaopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelperMissing(p) => write!(f, "MSA RAOP helper missing: {}", p.display()),
            Self::Spawn(e) => write!(f, "MSA RAOP helper spawn failed: {e}"),
            Self::Job(e) => write!(f, "MSA RAOP helper job setup failed: {e}"),
            Self::Pipe => write!(f, "MSA RAOP helper pipe missing"),
            Self::ReadinessTimeout => write!(f, "MSA RAOP helper readiness timed out"),
            Self::Connect(s) => write!(f, "MSA RAOP connect failed: {s}"),
            Self::Io(e) => write!(f, "MSA RAOP I/O failed: {e}"),
            Self::AckTimeout { seq } => write!(f, "MSA RAOP command {seq} timed out"),
            Self::Command { seq, command, detail } =>
                write!(f, "MSA RAOP command {seq} {command} failed: {detail}"),
            Self::InvalidAck(s) => write!(f, "invalid MSA RAOP ack: {s}"),
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

#[derive(Clone)]
pub struct MsaRaopPcmWriter {
    stdin: Arc<Mutex<ChildStdin>>,
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
        self.stdin.lock().map_err(|_| MsaRaopError::Pipe)?
            .write_all(packet).map_err(MsaRaopError::Io)
    }
}

pub struct MsaRaopSession {
    child: Child,
    job: KillOnCloseJob,
    stdin: Arc<Mutex<ChildStdin>>,
    control_path: PathBuf,
    ack_path: PathBuf,
    metadata_path: PathBuf,
    artwork_path: PathBuf,
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
        if let Some(bind_ip) = config.bind_ip {
            cmd.arg("--bind").arg(bind_ip.to_string());
        }
        if config.encrypt { cmd.arg("-e"); }
        if !config.compressed_alac { cmd.arg("--pcm"); }
        if config.mfi_auth { cmd.arg("-u"); }
        if let Some(secret) = config.secret.as_deref().filter(|v| !v.trim().is_empty()) {
            cmd.arg("-s").arg(secret);
        }
        if let Some(password) = config.password.as_deref().filter(|v| !v.is_empty()) {
            cmd.arg("-P").arg(password);
        }
        cmd.arg(&config.host)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd.spawn().map_err(MsaRaopError::Spawn)?;
        let job = match KillOnCloseJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => Arc::new(Mutex::new(stdin)),
            None => {
                job.terminate();
                let _ = child.wait();
                return Err(MsaRaopError::Pipe);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                job.terminate();
                let _ = child.wait();
                return Err(MsaRaopError::Pipe);
            }
        };

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<MsaRaopReady, String>>(1);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let head_audible_ms = Arc::new(AtomicU64::new(0));
        let log_t = Arc::clone(&log);
        let head_t = Arc::clone(&head_audible_ms);
        let log_thread = thread::Builder::new().name("msa-raop-log".into()).spawn(move || {
            let mut reported = false;
            let mut last_error = None::<String>;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut lines) = log_t.lock() {
                    if lines.len() >= 128 { lines.remove(0); }
                    lines.push(line.clone());
                }
                if let Some(rest) = line.strip_prefix("MSA-RAOP READY ") {
                    let mut latency = None;
                    let mut rate = None;
                    let mut depth = None;
                    let mut channels = None;
                    for token in rest.split_whitespace() {
                        if let Some(v) = token.strip_prefix("latency=") {
                            latency = v.parse::<u32>().ok();
                        } else if let Some(v) = token.strip_prefix("sample_rate=") {
                            rate = v.parse::<u32>().ok();
                        } else if let Some(v) = token.strip_prefix("bit_depth=") {
                            depth = v.parse::<u16>().ok();
                        } else if let Some(v) = token.strip_prefix("channels=") {
                            channels = v.parse::<u16>().ok();
                        }
                    }
                    if let (Some(latency_frames), Some(sample_rate), Some(bit_depth), Some(channels)) =
                        (latency, rate, depth, channels) {
                        let _ = ready_tx.send(Ok(MsaRaopReady {
                            latency_frames, sample_rate, bit_depth, channels
                        }));
                        reported = true;
                    }
                } else if let Some(rest) = line.strip_prefix("MSA-RAOP HEAD ") {
                    for token in rest.split_whitespace() {
                        if let Some(v) = token.strip_prefix("audible_ms=") {
                            if let Ok(ms) = v.parse::<u64>() {
                                head_t.store(ms, Ordering::SeqCst);
                            }
                        }
                    }
                } else if line.starts_with("MSA-RAOP ERROR ") {
                    last_error = Some(line);
                }
            }
            if !reported {
                let _ = ready_tx.send(Err(last_error.unwrap_or_else(|| {
                    "helper exited before reporting readiness".into()
                })));
            }
        });
        if let Err(error) = log_thread {
            job.terminate();
            let _ = child.wait();
            return Err(MsaRaopError::Spawn(error));
        }

        let ready = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                job.terminate();
                let _ = child.wait();
                return Err(MsaRaopError::Connect(e));
            }
            Err(_) => {
                job.terminate();
                let _ = child.wait();
                return Err(MsaRaopError::ReadinessTimeout);
            }
        };

        Ok(Self {
            child, job, stdin, control_path, ack_path, metadata_path, artwork_path,
            head_audible_ms,
            meta_delivered: false,
            meta_title: String::new(),
            meta_artist: String::new(),
            meta_album: String::new(),
            meta_duration_s: 0,
            meta_item_id: String::new(),
            next_seq: 1,
            state: MsaRaopState::Connected, ready, log,
        })
    }

    pub fn ready(&self) -> MsaRaopReady { self.ready }
    pub fn helper_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn state(&self) -> MsaRaopState { self.state }
    pub fn logs(&self) -> Vec<String> {
        self.log.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn head_audible_unix_ms(&self) -> u64 {
        self.head_audible_ms.load(Ordering::SeqCst)
    }

    pub fn commit_start(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        let at = self.command("START", requested_unix_ms, 0)?;
        self.state = MsaRaopState::Streaming;
        Ok(StartResolution {
            requested_unix_ms,
            at_unix_ms: at,
            corrected_forward: requested_unix_ms != 0 && at != requested_unix_ms,
        })
    }

    pub fn start_after_flush(&mut self, requested_unix_ms: u64) -> Result<StartResolution, MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        let at = self.command("START_AFTER_FLUSH", requested_unix_ms, 0)?;
        self.state = MsaRaopState::Streaming;
        Ok(StartResolution {
            requested_unix_ms,
            at_unix_ms: at,
            corrected_forward: requested_unix_ms != 0 && at != requested_unix_ms,
        })
    }

    pub fn flush(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        self.command("FLUSH", 0, 0)?;
        self.state = MsaRaopState::Flushed;
        Ok(())
    }
    pub fn standby(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        self.command("STANDBY", 0, 0)?;
        self.state = MsaRaopState::Connected;
        Ok(())
    }
    pub fn pause(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        self.command("PAUSE", 0, 0)?;
        self.state = MsaRaopState::Paused;
        Ok(())
    }
    pub fn play(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        self.command("PLAY", 0, 0)?;
        self.state = MsaRaopState::Streaming;
        Ok(())
    }
    pub fn stop(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        self.command("STOP", 0, 0)?;
        self.state = MsaRaopState::Down;
        Ok(())
    }
    pub fn set_volume(&mut self, percent: u8) -> Result<(), MsaRaopError> {
        self.command("VOLUME", percent.min(100) as u64, 0).map(|_| ())
    }
    pub fn set_progress(&mut self, elapsed_s: u32, duration_s: u32) -> Result<(), MsaRaopError> {
        self.command("PROGRESS", elapsed_s as u64, duration_s as u64).map(|_| ())
    }


    pub fn set_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: &str,
        duration_s: u32,
        item_id: &str,
    ) -> Result<(), MsaRaopError> {
        if self.meta_delivered
            && self.meta_title == title
            && self.meta_artist == artist
            && self.meta_album == album
            && self.meta_duration_s == duration_s
            && self.meta_item_id == item_id
        {
            return Ok(());
        }
        write_metadata_sidecar(&self.metadata_path, title, artist, album)?;
        self.command("METADATA", 0, 0)?;
        self.meta_delivered = true;
        self.meta_title = title.to_owned();
        self.meta_artist = artist.to_owned();
        self.meta_album = album.to_owned();
        self.meta_duration_s = duration_s;
        self.meta_item_id = item_id.to_owned();
        Ok(())
    }

    pub fn ensure_initial_metadata(&mut self) -> Result<(), MsaRaopError> {
        if self.meta_delivered {
            Ok(())
        } else {
            self.set_metadata("cliairplay", "", "", 0, "")
        }
    }

    pub fn set_artwork(&mut self, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
        write_artwork_sidecar(&self.artwork_path, content_type, data)?;
        self.command("ARTWORK", 0, 0).map(|_| ())
    }

    pub fn pcm_writer(&self) -> MsaRaopPcmWriter {
        MsaRaopPcmWriter {
            stdin: Arc::clone(&self.stdin),
            packet_bytes: RAOP_FRAMES_PER_PACKET
                * input_bytes_per_frame(self.ready.bit_depth, self.ready.channels),
        }
    }

    pub fn write_pcm_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        self.pcm_writer().write_packet(packet)
    }

    fn enqueue_command(&mut self, name: &str, arg1: u64, arg2: u64) -> Result<u64, MsaRaopError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);

        let tmp = self.control_path.with_extension("cmd.tmp");
        std::fs::write(&tmp, format!("{seq} {name} {arg1} {arg2}\n"))
            .map_err(MsaRaopError::Io)?;
        if self.control_path.exists() {
            let _ = std::fs::remove_file(&self.control_path);
        }
        std::fs::rename(&tmp, &self.control_path).map_err(MsaRaopError::Io)?;
        Ok(seq)
    }

    fn command(&mut self, name: &str, arg1: u64, arg2: u64) -> Result<u64, MsaRaopError> {
        let seq = self.enqueue_command(name, arg1, arg2)?;

        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            if let Ok(text) = std::fs::read_to_string(&self.ack_path) {
                if let Some((ack_seq, ok, at, detail)) = parse_ack(&text) {
                    if ack_seq == seq {
                        return if ok {
                            Ok(at)
                        } else {
                            Err(MsaRaopError::Command {
                                seq,
                                command: name.into(),
                                detail: detail.into(),
                            })
                        };
                    }
                }
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(MsaRaopError::Connect(format!(
                    "helper exited while waiting for {name}: {status}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(MsaRaopError::AckTimeout { seq });
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn disconnect(&mut self) {
        // Do not call command("QUIT") here: its normal command ACK timeout is
        // 12 seconds, which made a closed GUI appear gone while the 32-bit
        // helper still held the application directory. Queue QUIT once, allow
        // one bounded graceful teardown window, then terminate the whole job.
        let _ = self.enqueue_command("QUIT", 0, 0);
        let deadline = Instant::now() + DISCONNECT_GRACE;
        let mut exited = false;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        if !exited {
            self.job.terminate();
        }
        // Reap the process handle before Drop returns so cliraop-msa-solo.exe
        // cannot keep its executable/directory locked after the GUI is gone.
        let _ = self.child.wait();

        self.state = MsaRaopState::Down;
        let _ = std::fs::remove_file(&self.control_path);
        let _ = std::fs::remove_file(&self.ack_path);
        let _ = std::fs::remove_file(&self.metadata_path);
        let _ = std::fs::remove_file(&self.artwork_path);
    }
}
impl Drop for MsaRaopSession { fn drop(&mut self) { self.disconnect(); } }


fn write_u32_le(out: &mut Vec<u8>, value: usize) -> Result<(), MsaRaopError> {
    let value = u32::try_from(value).map_err(|_| {
        MsaRaopError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "sidecar field too large"))
    })?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_metadata_sidecar(path: &Path, title: &str, artist: &str, album: &str) -> Result<(), MsaRaopError> {
    let mut out = Vec::new();
    for value in [title.as_bytes(), artist.as_bytes(), album.as_bytes()] {
        write_u32_le(&mut out, value.len())?;
        out.extend_from_slice(value);
    }
    std::fs::write(path, out).map_err(MsaRaopError::Io)
}

fn write_artwork_sidecar(path: &Path, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
    let mut out = Vec::new();
    write_u32_le(&mut out, content_type.len())?;
    out.extend_from_slice(content_type.as_bytes());
    write_u32_le(&mut out, data.len())?;
    out.extend_from_slice(data);
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
}
