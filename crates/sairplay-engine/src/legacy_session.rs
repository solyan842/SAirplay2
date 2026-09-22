#![cfg(windows)]

use crate::{
    system_time_to_ntp, Pcm352Chunker, WasapiLoopbackCapture, WasapiLoopbackError,
    PCM352_PACKET_BYTES,
};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub const LIBRAOP_PINNED_COMMIT: &str = "81c2182649da8645ac2a58b78e9f370c79a4165b";
const RAOP_CONFIGURED_LATENCY_FRAMES: u32 = 44_100;
const RAOP_FIXED_LATENCY_FRAMES: u32 = 11_025;
const RAOP_GROUP_START_LEAD_MS: u64 = 5_000;
const WRITER_QUEUE_PACKETS: usize = 96;

#[derive(Debug, Clone)]
pub struct LegacyMemberConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub volume: u8,
    pub et: String,
    pub md: String,
    pub am: String,
    pub compressed_alac: bool,
    pub mfi_auth: bool,
}

impl LegacyMemberConfig {
    pub fn new(name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self {
            name: name.into(),
            host: host.into(),
            port,
            volume: 50,
            et: "0,4".into(),
            md: "0,1,2".into(),
            am: String::new(),
            compressed_alac: true,
            mfi_auth: false,
        }
    }
}

#[derive(Debug)]
pub enum LegacyGroupError {
    EmptyGroup,
    HelperMissing(PathBuf),
    Spawn { name: String, error: String },
    Capture(WasapiLoopbackError),
    Time(String),
}

impl fmt::Display for LegacyGroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup => write!(f, "legacy AirPlay group has no receivers"),
            Self::HelperMissing(path) => write!(
                f,
                "pinned libraop helper is missing: {}",
                path.display()
            ),
            Self::Spawn { name, error } => {
                write!(f, "{name}: cannot start libraop helper: {error}")
            }
            Self::Capture(error) => write!(f, "{error}"),
            Self::Time(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LegacyGroupError {}

struct SpawnedMember {
    name: String,
    pcm_tx: SyncSender<[u8; PCM352_PACKET_BYTES]>,
    writer: JoinHandle<()>,
}

pub struct LegacyGroupSession {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
    first_non_silent_frame: Arc<AtomicU64>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
}

impl LegacyGroupSession {
    pub fn connect(configs: Vec<LegacyMemberConfig>) -> Result<Self, LegacyGroupError> {
        if configs.is_empty() {
            return Err(LegacyGroupError::EmptyGroup);
        }

        let helper = helper_path()?;
        let now_ntp = system_time_to_ntp(SystemTime::now())
            .map_err(|error| LegacyGroupError::Time(format!(
                "NTP clock conversion failed: {error:?}"
            )))?;
        let start_ntp = now_ntp.saturating_add(ms_to_ntp(RAOP_GROUP_START_LEAD_MS));

        let running = Arc::new(AtomicBool::new(true));
        let last_error = Arc::new(Mutex::new(None));
        let discontinuities = Arc::new(AtomicU64::new(0));
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let first_non_silent_frame = Arc::new(AtomicU64::new(u64::MAX));
        let startup_events = Arc::new(Mutex::new(vec![format!(
            "Legacy transport: pinned libraop {} · shared audible START +{} ms.",
            LIBRAOP_PINNED_COMMIT, RAOP_GROUP_START_LEAD_MS
        )]));
        let active_members = Arc::new(AtomicU64::new(configs.len() as u64));

        let mut spawned = Vec::<SpawnedMember>::with_capacity(configs.len());
        for config in configs {
            spawned.push(spawn_member(
                &helper,
                config,
                start_ntp,
                Arc::clone(&running),
                Arc::clone(&last_error),
                Arc::clone(&startup_events),
                Arc::clone(&active_members),
            )?);
        }

        let running_thread = Arc::clone(&running);
        let error_thread = Arc::clone(&last_error);
        let disc_thread = Arc::clone(&discontinuities);
        let last_disc_thread = Arc::clone(&last_discontinuity_frame);
        let first_audio_thread = Arc::clone(&first_non_silent_frame);
        let events_thread = Arc::clone(&startup_events);
        let active_thread = Arc::clone(&active_members);

        let mut senders = spawned
            .iter()
            .map(|member| (member.name.clone(), member.pcm_tx.clone()))
            .collect::<Vec<_>>();
        let writers = spawned
            .drain(..)
            .map(|member| member.writer)
            .collect::<Vec<_>>();

        let feed_delay_ms = RAOP_GROUP_START_LEAD_MS.saturating_sub(
            frames_to_ms(RAOP_CONFIGURED_LATENCY_FRAMES + RAOP_FIXED_LATENCY_FRAMES)
                .saturating_add(100),
        );

        let worker = thread::Builder::new()
            .name("sairplay-legacy-audio".into())
            .spawn(move || {
                let delay_until = Instant::now() + Duration::from_millis(feed_delay_ms);
                while running_thread.load(Ordering::SeqCst) && Instant::now() < delay_until {
                    thread::sleep(Duration::from_millis(10));
                }

                if !running_thread.load(Ordering::SeqCst) {
                    drop(senders);
                    for writer in writers {
                        let _ = writer.join();
                    }
                    return;
                }

                let capture = match WasapiLoopbackCapture::open_default() {
                    Ok(capture) => capture,
                    Err(error) => {
                        if let Ok(mut slot) = error_thread.lock() {
                            *slot = Some(error.to_string());
                        }
                        running_thread.store(false, Ordering::SeqCst);
                        drop(senders);
                        for writer in writers {
                            let _ = writer.join();
                        }
                        return;
                    }
                };

                if let Ok(mut events) = events_thread.lock() {
                    events.push(format!(
                        "Legacy transport: WASAPI source opened {} ms before the shared audible anchor.",
                        RAOP_GROUP_START_LEAD_MS.saturating_sub(feed_delay_ms)
                    ));
                }

                let mut chunker = Pcm352Chunker::new();
                let mut captured_frames_total = 0u64;

                while running_thread.load(Ordering::SeqCst)
                    && active_thread.load(Ordering::SeqCst) != 0
                {
                    let report = match capture.drain_into(&mut chunker) {
                        Ok(report) => report,
                        Err(error) => {
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(error.to_string());
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };

                    if report.discontinuities != 0 {
                        disc_thread.fetch_add(report.discontinuities, Ordering::SeqCst);
                        if let Some(offset) = report.discontinuity_frame_offset {
                            last_disc_thread.store(
                                captured_frames_total.saturating_add(offset),
                                Ordering::SeqCst,
                            );
                        }
                    }
                    if let Some(offset) = report.first_non_silent_frame_offset {
                        let absolute = captured_frames_total.saturating_add(offset);
                        let _ = first_audio_thread.compare_exchange(
                            u64::MAX,
                            absolute,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                    }
                    captured_frames_total =
                        captured_frames_total.saturating_add(report.frames as u64);

                    while let Some(packet) = chunker.pop_packet() {
                        let mut index = 0usize;
                        while index < senders.len() {
                            let (name, tx) = &senders[index];
                            match tx.try_send(packet) {
                                Ok(()) => index += 1,
                                Err(TrySendError::Disconnected(_)) => {
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "{name}: libraop writer disconnected; removed from legacy group."
                                        ));
                                    }
                                    senders.remove(index);
                                }
                                Err(TrySendError::Full(_)) => {
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "{name}: libraop writer queue overrun; removed instead of dropping PCM."
                                        ));
                                    }
                                    senders.remove(index);
                                }
                            }
                        }
                    }

                    if report.frames == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }

                running_thread.store(false, Ordering::SeqCst);
                drop(senders);
                for writer in writers {
                    let _ = writer.join();
                }
            })
            .map_err(|error| LegacyGroupError::Capture(
                WasapiLoopbackError::Windows(format!(
                    "failed to spawn legacy WASAPI worker: {error}"
                )),
            ))?;

        Ok(Self {
            running,
            worker: Some(worker),
            last_error,
            discontinuities,
            last_discontinuity_frame,
            first_non_silent_frame,
            startup_events,
            active_members,
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
            && self.active_members.load(Ordering::SeqCst) != 0
    }

    pub fn active_members(&self) -> usize {
        self.active_members.load(Ordering::SeqCst) as usize
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn discontinuity_count(&self) -> u64 {
        self.discontinuities.load(Ordering::SeqCst)
    }

    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        match self.last_discontinuity_frame.load(Ordering::SeqCst) {
            u64::MAX => None,
            value => Some(value),
        }
    }

    pub fn first_non_silent_frame(&self) -> Option<u64> {
        match self.first_non_silent_frame.load(Ordering::SeqCst) {
            u64::MAX => None,
            value => Some(value),
        }
    }

    pub fn drain_startup_events(&self) -> Vec<String> {
        self.startup_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for LegacyGroupSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_member(
    helper: &Path,
    config: LegacyMemberConfig,
    start_ntp: u64,
    running: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
) -> Result<SpawnedMember, LegacyGroupError> {
    let mut command = Command::new(helper);
    command
        .arg("-p")
        .arg(config.port.to_string())
        .arg("-v")
        .arg(config.volume.min(100).to_string())
        .arg("-l")
        .arg(RAOP_CONFIGURED_LATENCY_FRAMES.to_string())
        .arg("-n")
        .arg(start_ntp.to_string())
        .arg("-t")
        .arg(&config.et)
        .arg("-m")
        .arg(&config.md)
        .arg("-d")
        .arg("3");

    if config.compressed_alac {
        command.arg("-a");
    }
    if config.mfi_auth {
        command.arg("-u");
    }

    command
        .arg(&config.host)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(0x08000000);

    let mut child = command.spawn().map_err(|error| LegacyGroupError::Spawn {
        name: config.name.clone(),
        error: error.to_string(),
    })?;

    let stdin = child.stdin.take().ok_or_else(|| LegacyGroupError::Spawn {
        name: config.name.clone(),
        error: "helper stdin pipe was not created".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| LegacyGroupError::Spawn {
        name: config.name.clone(),
        error: "helper stderr pipe was not created".into(),
    })?;

    let (connected_tx, connected_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let reader_name = config.name.clone();
    let reader_events = Arc::clone(&startup_events);
    thread::Builder::new()
        .name("sairplay-libraop-log".into())
        .spawn(move || {
            let mut connected = false;
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let lower = line.to_ascii_lowercase();
                if !connected && lower.contains("connected to") {
                    connected = true;
                    let _ = connected_tx.send(Ok(()));
                    if let Ok(mut events) = reader_events.lock() {
                        events.push(format!("{reader_name}: libraop connected."));
                    }
                } else if lower.contains("error")
                    || lower.contains("failed")
                    || lower.contains("cannot connect")
                {
                    if let Ok(mut events) = reader_events.lock() {
                        events.push(format!("{reader_name}: {line}"));
                    }
                }
            }
            if !connected {
                let _ = connected_tx.send(Err(
                    "helper exited before reporting raopcl_connect readiness".into(),
                ));
            }
        })
        .map_err(|error| LegacyGroupError::Spawn {
            name: config.name.clone(),
            error: format!("cannot spawn helper log reader: {error}"),
        })?;

    let gate_name = config.name.clone();
    let gate_running = Arc::clone(&running);
    let gate_error = Arc::clone(&last_error);
    let gate_events = Arc::clone(&startup_events);
    thread::Builder::new()
        .name("sairplay-libraop-ready".into())
        .spawn(move || match connected_rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let message = format!("{gate_name}: {error}");
                if let Ok(mut slot) = gate_error.lock() {
                    *slot = Some(message.clone());
                }
                if let Ok(mut events) = gate_events.lock() {
                    events.push(message);
                }
                gate_running.store(false, Ordering::SeqCst);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let message =
                    format!("{gate_name}: helper readiness channel disconnected");
                if let Ok(mut slot) = gate_error.lock() {
                    *slot = Some(message.clone());
                }
                if let Ok(mut events) = gate_events.lock() {
                    events.push(message);
                }
                gate_running.store(false, Ordering::SeqCst);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let message = format!("{gate_name}: libraop connect readiness timed out");
                if let Ok(mut slot) = gate_error.lock() {
                    *slot = Some(message.clone());
                }
                if let Ok(mut events) = gate_events.lock() {
                    events.push(message);
                }
                gate_running.store(false, Ordering::SeqCst);
            }
        })
        .map_err(|error| LegacyGroupError::Spawn {
            name: config.name.clone(),
            error: format!("cannot spawn helper readiness gate: {error}"),
        })?;

    let (pcm_tx, pcm_rx) =
        mpsc::sync_channel::<[u8; PCM352_PACKET_BYTES]>(WRITER_QUEUE_PACKETS);
    let writer_name = config.name.clone();
    let writer_events = Arc::clone(&startup_events);
    let writer_running = Arc::clone(&running);
    let writer_error = Arc::clone(&last_error);
    let writer_active = Arc::clone(&active_members);

    let writer = thread::Builder::new()
        .name("sairplay-libraop-pcm".into())
        .spawn(move || {
            legacy_writer_loop(
                writer_name,
                &mut child,
                stdin,
                pcm_rx,
                writer_running,
                writer_error,
                writer_events,
                writer_active,
            );
        })
        .map_err(|error| LegacyGroupError::Spawn {
            name: config.name.clone(),
            error: format!("cannot spawn helper PCM writer: {error}"),
        })?;

    Ok(SpawnedMember {
        name: config.name,
        pcm_tx,
        writer,
    })
}

fn legacy_writer_loop(
    name: String,
    child: &mut Child,
    mut stdin: ChildStdin,
    pcm_rx: Receiver<[u8; PCM352_PACKET_BYTES]>,
    running: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
) {
    while running.load(Ordering::SeqCst) {
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Ok(mut events) = startup_events.lock() {
                    events.push(format!("{name}: libraop exited with {status}."));
                }
                break;
            }
            Ok(None) => {}
            Err(error) => {
                if let Ok(mut events) = startup_events.lock() {
                    events.push(format!("{name}: helper status check failed: {error}."));
                }
                break;
            }
        }

        match pcm_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => {
                if let Err(error) = stdin.write_all(&packet) {
                    if let Ok(mut events) = startup_events.lock() {
                        events.push(format!("{name}: PCM pipe failed: {error}."));
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    let previous = active_members.fetch_sub(1, Ordering::SeqCst);
    if previous <= 1 {
        if let Ok(mut slot) = last_error.lock() {
            if slot.is_none() && running.load(Ordering::SeqCst) {
                *slot = Some("all legacy AirPlay members stopped".into());
            }
        }
        running.store(false, Ordering::SeqCst);
    }
}

fn helper_path() -> Result<PathBuf, LegacyGroupError> {
    let exe = std::env::current_exe()
        .map_err(|_| LegacyGroupError::HelperMissing(PathBuf::from("cliraop.exe")))?;
    let path = exe
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cliraop.exe");
    if path.is_file() {
        Ok(path)
    } else {
        Err(LegacyGroupError::HelperMissing(path))
    }
}

fn ms_to_ntp(ms: u64) -> u64 {
    ((ms as u128) << 32).div_ceil(1000) as u64
}

fn frames_to_ms(frames: u32) -> u64 {
    (frames as u64 * 1000).div_ceil(44_100)
}
