#![cfg(windows)]

//! Concrete Windows RAOP transport for the independent MSA SOLO engine.
//! Owns a separate helper built from the exact libraop pin used by pinned MSA.
//! The helper process and raopcl_s persist across FLUSH/START; legacy engine
//! helpers are not touched or reused.

use crate::timing::StartResolution;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
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
const COMMAND_TIMEOUT: Duration = Duration::from_secs(12);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const CREATE_NO_WINDOW: u32 = 0x08000000;
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaRaopState { Streaming, Flushed, Stopped, Down }

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
        }
    }
}
impl std::error::Error for MsaRaopError {}

#[derive(Debug, Clone, Copy)]
pub struct MsaRaopReady {
    pub latency_frames: u32,
    pub sample_rate: u32,
}

#[derive(Clone)]
pub struct MsaRaopPcmWriter {
    stdin: Arc<Mutex<ChildStdin>>,
}
impl MsaRaopPcmWriter {
    pub fn write_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        if packet.len() != RAOP_PCM_PACKET_BYTES {
            return Err(MsaRaopError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("RAOP PCM packet must be {RAOP_PCM_PACKET_BYTES} bytes"),
            )));
        }
        self.stdin.lock().map_err(|_| MsaRaopError::Pipe)?
            .write_all(packet).map_err(MsaRaopError::Io)
    }
}

pub struct MsaRaopSession {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    control_path: PathBuf,
    ack_path: PathBuf,
    metadata_path: PathBuf,
    artwork_path: PathBuf,
    head_audible_ms: Arc<AtomicU64>,
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
            .arg("-l").arg("44100")
            .arg("-t").arg(&config.et)
            .arg("-m").arg(&config.md);
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
        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or(MsaRaopError::Pipe)?));
        let stderr = child.stderr.take().ok_or(MsaRaopError::Pipe)?;

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
                    let mut latency = None;
                    let mut rate = None;
                    for token in rest.split_whitespace() {
                        if let Some(v) = token.strip_prefix("latency=") {
                            latency = v.parse::<u32>().ok();
                        } else if let Some(v) = token.strip_prefix("sample_rate=") {
                            rate = v.parse::<u32>().ok();
                        }
                    }
                    if let (Some(latency_frames), Some(sample_rate)) = (latency, rate) {
                        let _ = ready_tx.send(Ok(MsaRaopReady { latency_frames, sample_rate }));
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
        }).map_err(MsaRaopError::Spawn)?;

        let ready = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let _ = child.kill();
                return Err(MsaRaopError::Connect(e));
            }
            Err(_) => {
                let _ = child.kill();
                return Err(MsaRaopError::ReadinessTimeout);
            }
        };

        Ok(Self {
            child, stdin, control_path, ack_path, metadata_path, artwork_path,
            head_audible_ms, next_seq: 1,
            state: MsaRaopState::Streaming, ready, log,
        })
    }

    pub fn ready(&self) -> MsaRaopReady { self.ready }
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
        self.state = MsaRaopState::Flushed;
        Ok(())
    }
    pub fn pause(&mut self) -> Result<(), MsaRaopError> {
        self.head_audible_ms.store(0, Ordering::SeqCst);
        self.command("PAUSE", 0, 0)?;
        self.state = MsaRaopState::Flushed;
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
        self.state = MsaRaopState::Stopped;
        Ok(())
    }
    pub fn set_volume(&mut self, percent: u8) -> Result<(), MsaRaopError> {
        self.command("VOLUME", percent.min(100) as u64, 0).map(|_| ())
    }
    pub fn set_progress(&mut self, elapsed_s: u32, duration_s: u32) -> Result<(), MsaRaopError> {
        self.command("PROGRESS", elapsed_s as u64, duration_s as u64).map(|_| ())
    }


    pub fn set_metadata(&mut self, title: &str, artist: &str, album: &str) -> Result<(), MsaRaopError> {
        write_metadata_sidecar(&self.metadata_path, title, artist, album)?;
        self.command("METADATA", 0, 0).map(|_| ())
    }

    pub fn set_artwork(&mut self, content_type: &str, data: &[u8]) -> Result<(), MsaRaopError> {
        write_artwork_sidecar(&self.artwork_path, content_type, data)?;
        self.command("ARTWORK", 0, 0).map(|_| ())
    }

    pub fn pcm_writer(&self) -> MsaRaopPcmWriter {
        MsaRaopPcmWriter { stdin: Arc::clone(&self.stdin) }
    }

    pub fn write_pcm_packet(&self, packet: &[u8]) -> Result<(), MsaRaopError> {
        self.pcm_writer().write_packet(packet)
    }

    fn command(&mut self, name: &str, arg1: u64, arg2: u64) -> Result<u64, MsaRaopError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);

        let tmp = self.control_path.with_extension("cmd.tmp");
        std::fs::write(&tmp, format!("{seq} {name} {arg1} {arg2}\n"))
            .map_err(MsaRaopError::Io)?;
        if self.control_path.exists() {
            let _ = std::fs::remove_file(&self.control_path);
        }
        std::fs::rename(&tmp, &self.control_path).map_err(MsaRaopError::Io)?;

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
        let _ = self.command("QUIT", 0, 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) { break; }
            thread::sleep(Duration::from_millis(10));
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
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
