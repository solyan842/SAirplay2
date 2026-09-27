#![cfg(windows)]

use crate::{
    cross_transport_timeline::AIRPLAY_COLD_GROUP_START_LEAD_MS,
    group_start_orchestrator::{
        run_concurrent_group_start_round, run_group_start_convergence,
        GroupStartIoError, GroupStartParticipant,
    },
    group_flush::{parse_group_flush_status, GroupFlushAck},
    group_pcm_fanout::GroupPcmSource,
    system_time_to_ntp, volume_percent_to_db, Ap2AudioFormat, VolumeSetResult,
    WasapiLoopbackError, WindowsPcmSession, PCM352_PACKET_BYTES,
};
use std::fmt;
use std::fs::{File, OpenOptions};
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const LIBRAOP_PINNED_COMMIT: &str = "dadcfcaa26d988cdd3e3501ddf8286c224f1b494";
const RAOP_CONFIGURED_LATENCY_FRAMES: u32 = 44_100;
const RAOP_FIXED_LATENCY_FRAMES: u32 = 11_025;
const WRITER_QUEUE_PACKETS: usize = 96;
const RAOP_START_ACK_TIMEOUT: Duration = Duration::from_millis(7_000);
const RAOP_FLUSH_ACK_TIMEOUT: Duration = Duration::from_millis(2_000);
// Music Assistant's current source treats a player that does not consume its
// PCM feed for 35 s as a failed member. Do the same here, but preserve every
// PCM packet until that deadline instead of guessing a larger queue.
const WRITER_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(35);
static LEGACY_VOLUME_FILE_ID: AtomicU64 = AtomicU64::new(1);
static LEGACY_COMMAND_PIPE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct LegacyMemberConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub volume: u8,
    pub et: String,
    pub md: String,
    pub am: String,
    pub pk: String,
    pub cn: String,
    pub secret: Option<String>,
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
            pk: String::new(),
            cn: String::new(),
            secret: None,
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
    Connect { name: String, error: String },
    Start { name: String, error: String },
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
            Self::Connect { name, error } => {
                write!(f, "{name}: libraop did not reach connected state: {error}")
            }
            Self::Start { name, error } => {
                write!(f, "{name}: libraop did not commit commanded START: {error}")
            }
            Self::Capture(error) => write!(f, "{error}"),
            Self::Time(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LegacyGroupError {}

#[derive(Clone)]
pub struct LegacyVolumeControl {
    path: PathBuf,
}

impl LegacyVolumeControl {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn set(&self, percent: u8) -> Result<VolumeSetResult, std::io::Error> {
        let percent = percent.min(100);
        // The source helper polls this tiny control file and applies the value
        // through libraop's own raopcl_set_volume() on the live RTSP session.
        // Keep the file write atomic enough for the helper's integer parser.
        std::fs::write(&self.path, format!("{}\n", percent))?;
        Ok(VolumeSetResult {
            percent,
            db: volume_percent_to_db(percent),
            // 0 means queued to the source-built legacy helper. Native RTSP
            // responses use real HTTP/RTSP status codes and never report 0.
            status: 0,
        })
    }
}

enum LegacyWriterCommand {
    Quiesce(SyncSender<Result<(), String>>),
    Resume,
}

struct SpawnedMember {
    name: String,
    pid: u32,
    pcm_tx: SyncSender<[u8; PCM352_PACKET_BYTES]>,
    writer_control_tx: SyncSender<LegacyWriterCommand>,
    connected_rx: Receiver<Result<(), String>>,
    started_rx: Option<Receiver<Result<(u64, u64), String>>>,
    flushed_rx: Option<Receiver<Result<GroupFlushAck, String>>>,
    command_pipe: Option<File>,
    writer: JoinHandle<()>,
    volume_control: LegacyVolumeControl,
}

impl GroupStartParticipant for SpawnedMember {
    fn name(&self) -> &str {
        &self.name
    }

    fn start_at_unix_ms(&mut self, requested_start_unix_ms: u64) -> Result<u64, String> {
        let pipe = self
            .command_pipe
            .as_mut()
            .ok_or_else(|| "runtime command pipe missing".to_owned())?;
        let rx = self
            .started_rx
            .as_ref()
            .ok_or_else(|| "START acknowledgement channel missing".to_owned())?;

        send_start_command(pipe, requested_start_unix_ms)
            .map_err(|error| format!("cannot send START command: {error}"))?;

        let ack = rx.recv_timeout(RAOP_START_ACK_TIMEOUT).map_err(|error| {
            format!(
                "START acknowledgement not received within {} ms: {}",
                RAOP_START_ACK_TIMEOUT.as_millis(),
                error
            )
        })?;
        let (requested, actual) = ack?;

        if requested != requested_start_unix_ms {
            return Err(format!(
                "helper acknowledged request {} instead of {}",
                requested, requested_start_unix_ms
            ));
        }

        Ok(actual)
    }
}

struct LegacyCommandControl {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    pipe: File,
    #[allow(dead_code)]
    writer_control_tx: SyncSender<LegacyWriterCommand>,
    #[allow(dead_code)]
    started_rx: Receiver<Result<(u64, u64), String>>,
    #[allow(dead_code)]
    flushed_rx: Receiver<Result<GroupFlushAck, String>>,
}

impl LegacyCommandControl {
    /// Execute the exact parent-side FLUSH transaction used by current Music
    /// Assistant: hold stdin quiet, send ACTION=FLUSH out-of-band, wait for the
    /// binary acknowledgement, then release stdin again.
    ///
    /// The caller is responsible for defining the content boundary before
    /// entering this function. For track-based MA that means "old feeder
    /// stopped". SAirplay2's continuous WASAPI path does not yet expose this
    /// method because inventing where to cut live system audio would diverge
    /// from source behavior.
    #[allow(dead_code)]
    fn flush_quiesced(&mut self) -> Result<GroupFlushAck, String> {
        quiesce_legacy_writer(&self.writer_control_tx)?;

        let transaction = (|| -> Result<GroupFlushAck, String> {
            send_flush_command(&mut self.pipe)
                .map_err(|error| format!("cannot send ACTION=FLUSH: {error}"))?;

            let ack = self
                .flushed_rx
                .recv_timeout(RAOP_FLUSH_ACK_TIMEOUT)
                .map_err(|error| {
                    format!(
                        "FLUSH acknowledgement not received within {} ms: {}",
                        RAOP_FLUSH_ACK_TIMEOUT.as_millis(),
                        error
                    )
                })?;

            ack
        })();

        // Match the async-context-manager lifetime in Music Assistant:
        // stdin becomes writable again regardless of whether FLUSH succeeded,
        // timed out or was rejected. The caller decides whether to cold restart.
        let resume = resume_legacy_writer(&self.writer_control_tx);
        match (transaction, resume) {
            (Ok(head), Ok(())) => Ok(head),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(first), Err(resume_error)) => Err(format!(
                "{first}; additionally failed to resume PCM writer: {resume_error}"
            )),
        }
    }
}

pub struct LegacyGroupSession {
    running: Arc<AtomicBool>,
    helper_pids: Vec<u32>,
    worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    discontinuities: Arc<AtomicU64>,
    last_discontinuity_frame: Arc<AtomicU64>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
    volume_controls: Vec<LegacyVolumeControl>,
    // Keep the MSA-style Windows command channel and acknowledgements alive
    // for the whole persistent helper lifetime. FLUSH is deliberately not
    // exposed to callers yet: MSA requires stdin/session-ring quiescing before
    // FLUSH, and the legacy producer layer is normalized in the next step.
    _command_controls: Vec<LegacyCommandControl>,
}

impl LegacyGroupSession {
    pub fn connect(configs: Vec<LegacyMemberConfig>) -> Result<Self, LegacyGroupError> {
        if configs.is_empty() {
            return Err(LegacyGroupError::EmptyGroup);
        }

        let member_count = configs.len();
        let scheduled_group_start = member_count > 1;
        let helper = helper_path()?;

        let running = Arc::new(AtomicBool::new(true));
        let last_error = Arc::new(Mutex::new(None));
        let discontinuities = Arc::new(AtomicU64::new(0));
        let last_discontinuity_frame = Arc::new(AtomicU64::new(u64::MAX));
        let startup_events = Arc::new(Mutex::new(Vec::new()));
        let active_members = Arc::new(AtomicU64::new(configs.len() as u64));

        let mut spawned = Vec::<SpawnedMember>::with_capacity(configs.len());
        for config in configs {
            spawned.push(spawn_member(
                &helper,
                config,
                scheduled_group_start,
                Arc::clone(&running),
                Arc::clone(&last_error),
                Arc::clone(&startup_events),
                Arc::clone(&active_members),
            )?);
        }

        // Source/controller contract: a group is not Running until every member
        // has completed raopcl_connect(). This also matches the GUI rule that
        // transport health is proven before the state flips to playing.
        let mut readiness_error: Option<(String, String)> = None;
        for member in &spawned {
            // Source-aligned readiness: wait for the helper's actual
            // raopcl_connect() result. The pinned RTSP layer uses a 10 s
            // response timeout per exchange; an outer 8 s timeout was shorter
            // than the source contract and caused false failures on TV-class
            // receivers.
            match member.connected_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    readiness_error = Some((member.name.clone(), error));
                    break;
                }
                Err(error) => {
                    readiness_error = Some((member.name.clone(), error.to_string()));
                    break;
                }
            }
        }

        if let Some((name, error)) = readiness_error {
            running.store(false, Ordering::SeqCst);
            for member in &spawned {
                kill_helper_tree(member.pid);
            }
            for member in spawned {
                drop(member.pcm_tx);
                let _ = member.writer.join();
            }
            return Err(LegacyGroupError::Connect { name, error });
        }

        // MSA source order: establish every receiver connection first, then
        // fan one shared audible START out to every member. Each round waits for
        // all TRUE scheduled-instant acknowledgements concurrently, removes any
        // per-member sync adjustment (legacy currently has none, so 0), and
        // feeds the common cross-transport convergence contract. A correction
        // re-anchors every member at largest_ack + 150 ms for at most four
        // rounds; if the fourth round still corrects, retain where members
        // actually landed rather than recording a retry that was never sent.
        let start_result = (|| -> Result<Option<u64>, LegacyGroupError> {
            if !scheduled_group_start {
                return Ok(None);
            }

            let initial_start_unix_ms = current_unix_ms()
                .map_err(LegacyGroupError::Time)?
                .saturating_add(AIRPLAY_COLD_GROUP_START_LEAD_MS);
            let mut participants = spawned
                .iter_mut()
                .map(|member| member as &mut dyn GroupStartParticipant)
                .collect::<Vec<_>>();

            let convergence = run_group_start_convergence(
                initial_start_unix_ms,
                |target_unix_ms| {
                    let mut round_members: Vec<&mut dyn GroupStartParticipant> =
                        Vec::with_capacity(participants.len());
                    for member in participants.iter_mut() {
                        round_members.push(&mut **member);
                    }
                    run_concurrent_group_start_round(round_members, target_unix_ms)
                },
            )
            .map_err(|error: GroupStartIoError| LegacyGroupError::Start {
                name: error.member,
                error: error.error,
            })?;

            for correction in &convergence.corrections {
                if let Ok(mut events) = startup_events.lock() {
                    events.push(format!(
                        "Legacy group START corrected: round {}/{} · requested={} · largest_ack={} · retry={}.",
                        correction.round,
                        crate::cross_transport_timeline::AIRPLAY_START_CONVERGENCE_MAX_ROUNDS,
                        correction.requested_unix_ms,
                        correction.corrected_unix_ms,
                        correction.retry_unix_ms
                    ));
                }
            }

            if !convergence.converged {
                if let Ok(mut events) = startup_events.lock() {
                    events.push(format!(
                        "Legacy group START did not converge after {} rounds · retaining last acknowledged instant {}.",
                        crate::cross_transport_timeline::AIRPLAY_START_CONVERGENCE_MAX_ROUNDS,
                        convergence.anchor_unix_ms
                    ));
                }
            }

            if let Ok(mut events) = startup_events.lock() {
                events.push(format!(
                    "Legacy group START committed after all members connected · initial={} · committed={} · rounds={} · lead={} ms.",
                    initial_start_unix_ms,
                    convergence.anchor_unix_ms,
                    convergence.rounds,
                    AIRPLAY_COLD_GROUP_START_LEAD_MS
                ));
            }
            Ok(Some(convergence.anchor_unix_ms))
        })();

        let committed_group_start_unix_ms = match start_result {
            Ok(value) => value,
            Err(error) => {
                running.store(false, Ordering::SeqCst);
                for member in &spawned {
                    kill_helper_tree(member.pid);
                }
                for member in spawned {
                    drop(member.pcm_tx);
                    let _ = member.writer.join();
                }
                return Err(error);
            }
        };

        let running_thread = Arc::clone(&running);
        let error_thread = Arc::clone(&last_error);
        let disc_thread = Arc::clone(&discontinuities);
        let last_disc_thread = Arc::clone(&last_discontinuity_frame);
        let events_thread = Arc::clone(&startup_events);
        let active_thread = Arc::clone(&active_members);

        let helper_pids = spawned.iter().map(|member| member.pid).collect::<Vec<_>>();
        let volume_controls = spawned
            .iter()
            .map(|member| member.volume_control.clone())
            .collect::<Vec<_>>();
        let mut senders = spawned
            .iter()
            .map(|member| (member.name.clone(), member.pcm_tx.clone()))
            .collect::<Vec<_>>();
        let command_controls = spawned
            .iter_mut()
            .filter_map(|member| {
                let pipe = member.command_pipe.take()?;
                let started_rx = member.started_rx.take()?;
                let flushed_rx = member.flushed_rx.take()?;
                Some(LegacyCommandControl {
                    name: member.name.clone(),
                    pipe,
                    writer_control_tx: member.writer_control_tx.clone(),
                    started_rx,
                    flushed_rx,
                })
            })
            .collect::<Vec<_>>();
        let writers = spawned
            .drain(..)
            .map(|member| member.writer)
            .collect::<Vec<_>>();

        // Solo keeps the already-validated immediate libraop path. A real
        // group is now commanded only after connection readiness, and the PCM
        // feed begins one configured+fixed receiver-latency window before the
        // verified audible instant, as on the existing libraop transport.
        let feed_ntp = committed_group_start_unix_ms.map(|start_unix_ms| {
            let total_latency_frames =
                RAOP_CONFIGURED_LATENCY_FRAMES + RAOP_FIXED_LATENCY_FRAMES;
            unix_ms_to_ntp(start_unix_ms)
                .saturating_sub(frames_to_ntp(total_latency_frames))
        });

        let worker = thread::Builder::new()
            .name("sairplay-legacy-audio".into())
            .spawn(move || {
                if let Some(feed_ntp) = feed_ntp {
                    while running_thread.load(Ordering::SeqCst) {
                        let now_ntp = match system_time_to_ntp(SystemTime::now()) {
                            Ok(value) => value,
                            Err(error) => {
                                if let Ok(mut slot) = error_thread.lock() {
                                    *slot = Some(format!(
                                        "legacy feed clock conversion failed: {error:?}"
                                    ));
                                }
                                running_thread.store(false, Ordering::SeqCst);
                                break;
                            }
                        };
                        if now_ntp >= feed_ntp {
                            break;
                        }
                        let remaining_ms = ntp_delta_to_ms(feed_ntp - now_ntp);
                        thread::sleep(Duration::from_millis(remaining_ms.clamp(1, 10)));
                    }
                }

                if !running_thread.load(Ordering::SeqCst) {
                    drop(senders);
                    for writer in writers {
                        let _ = writer.join();
                    }
                    return;
                }

                let mut pcm_session = match WindowsPcmSession::start(
                    Ap2AudioFormat::ALAC_44100_16_STEREO,
                ) {
                    Ok(session) => session,
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

                let mut captured_frames_seen = 0u64;

                while running_thread.load(Ordering::SeqCst)
                    && active_thread.load(Ordering::SeqCst) != 0
                {
                    for event in pcm_session.drain_discontinuity_events() {
                        disc_thread.store(event.cumulative, Ordering::SeqCst);
                        if let Some(frame) = event.absolute_frame {
                            last_disc_thread.store(frame, Ordering::SeqCst);
                        }
                    }

                    let captured_frames_now = pcm_session.captured_frames();
                    let frames = captured_frames_now.saturating_sub(captured_frames_seen);
                    captured_frames_seen = captured_frames_now;

                    let packet = match GroupPcmSource::read_shared_pcm(
                        &pcm_session,
                        PCM352_PACKET_BYTES,
                        Duration::from_millis(0),
                    ) {
                        Ok(Some(packet)) => {
                            let packet: [u8; PCM352_PACKET_BYTES] = packet
                                .try_into()
                                .expect("legacy shared PCM source returned exact packet size");
                            packet
                        }
                        Ok(None) => {
                            if frames == 0 {
                                thread::sleep(Duration::from_millis(1));
                            }
                            continue;
                        }
                        Err(error) => {
                            if let Ok(mut slot) = error_thread.lock() {
                                *slot = Some(error);
                            }
                            running_thread.store(false, Ordering::SeqCst);
                            break;
                        }
                    };

                    // Legacy RAOP remains locked to the 16-bit / 44.1 kHz
                    // 1408-byte packet contract. Only source ownership moved
                    // behind the common GroupPcmSource boundary.
                        let mut index = 0usize;
                        while index < senders.len() {
                            let stall_started = std::time::Instant::now();
                            loop {
                                let (name, tx) = &senders[index];
                                match tx.try_send(packet) {
                                    Ok(()) => {
                                        index += 1;
                                        break;
                                    }
                                    Err(TrySendError::Disconnected(_)) => {
                                        if let Ok(mut events) = events_thread.lock() {
                                            events.push(format!(
                                                "{name}: libraop writer disconnected; removed from legacy group."
                                            ));
                                        }
                                        senders.remove(index);
                                        break;
                                    }
                                    Err(TrySendError::Full(_)) => {
                                        // cliraop/libraop is pull-paced by
                                        // raopcl_accept_frames(). A full queue
                                        // is therefore normal backpressure, not
                                        // packet loss. Hold this exact packet
                                        // until the source consumes it. Only
                                        // evict after the same 35 s write
                                        // timeout used by Music Assistant.
                                        if !running_thread.load(Ordering::SeqCst) {
                                            break;
                                        }
                                        if stall_started.elapsed() >= WRITER_BACKPRESSURE_TIMEOUT {
                                            if let Ok(mut events) = events_thread.lock() {
                                                events.push(format!(
                                                    "{name}: stopped reading PCM for 35 s; removed from legacy group."
                                                ));
                                            }
                                            senders.remove(index);
                                            break;
                                        }
                                        thread::sleep(Duration::from_millis(1));
                                    }
                                }
                            }
                            if !running_thread.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                }

                pcm_session.stop();
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
            helper_pids,
            worker: Some(worker),
            last_error,
            discontinuities,
            last_discontinuity_frame,
            startup_events,
            active_members,
            volume_controls,
            _command_controls: command_controls,
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
            && self.active_members.load(Ordering::SeqCst) != 0
    }

    pub fn active_members(&self) -> usize {
        self.active_members.load(Ordering::SeqCst) as usize
    }

    pub fn volume_controls(&self) -> Vec<LegacyVolumeControl> {
        self.volume_controls.clone()
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

    pub fn drain_startup_events(&self) -> Vec<String> {
        self.startup_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        // Prefer the source's normal EOF -> drain -> raopcl_disconnect path.
        // Keep the #594 anti-hang guarantee with a delayed watchdog: if a
        // helper is still blocked in stdin/RTSP after two seconds, terminate
        // that local process tree to unblock the Rust writer.
        let helper_pids = std::mem::take(&mut self.helper_pids);
        let watchdog = if helper_pids.is_empty() {
            None
        } else {
            Some(thread::spawn(move || {
                thread::sleep(Duration::from_secs(2));
                for pid in helper_pids {
                    kill_helper_tree(pid);
                }
            }))
        };

        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(watchdog) = watchdog {
            let _ = watchdog.join();
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
    command_session: bool,
    running: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
) -> Result<SpawnedMember, LegacyGroupError> {
    let volume_path = std::env::temp_dir().join(format!(
        "sairplay2-legacy-volume-{}-{}.txt",
        std::process::id(),
        LEGACY_VOLUME_FILE_ID.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::write(&volume_path, format!("{}\n", config.volume.min(100)))
        .map_err(|error| LegacyGroupError::Spawn {
            name: config.name.clone(),
            error: format!("cannot initialize legacy volume control: {error}"),
        })?;

    let command_pipe_name = command_session.then(|| {
        format!(
            r"\\.\pipe\sairplay2-raop-{}-{}",
            std::process::id(),
            LEGACY_COMMAND_PIPE_ID.fetch_add(1, Ordering::SeqCst)
        )
    });

    let mut command = Command::new(helper);
    command
        .arg("-p")
        .arg(config.port.to_string())
        .arg("-v")
        .arg(config.volume.min(100).to_string())
        .arg("-V")
        .arg(&volume_path)
        .arg("-l")
        .arg(RAOP_CONFIGURED_LATENCY_FRAMES.to_string())
        .arg("-t")
        .arg(&config.et)
        .arg("-m")
        .arg(&config.md)
        .arg("-d")
        .arg("3");

    if let Some(pipe_name) = command_pipe_name.as_deref() {
        command.arg("-C").arg(pipe_name);
    }

    if config.compressed_alac {
        command.arg("-a");
    }
    if config.mfi_auth {
        command.arg("-u");
    }
    if let Some(secret) = config.secret.as_deref() {
        if !secret.trim().is_empty() {
            command.arg("-s").arg(secret);
        }
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
    let child_pid = child.id();

    let command_pipe = if let Some(pipe_name) = command_pipe_name.as_deref() {
        Some(open_command_pipe_writer(pipe_name).map_err(|error| LegacyGroupError::Spawn {
            name: config.name.clone(),
            error: format!("cannot attach runtime command pipe: {error}"),
        })?)
    } else {
        None
    };

    let stdin = child.stdin.take().ok_or_else(|| LegacyGroupError::Spawn {
        name: config.name.clone(),
        error: "helper stdin pipe was not created".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| LegacyGroupError::Spawn {
        name: config.name.clone(),
        error: "helper stderr pipe was not created".into(),
    })?;

    let (connected_tx, connected_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let (started_tx, started_rx) = mpsc::sync_channel::<Result<(u64, u64), String>>(4);
    let (flushed_tx, flushed_rx) = mpsc::sync_channel::<Result<GroupFlushAck, String>>(4);
    let reader_name = config.name.clone();
    let reader_events = Arc::clone(&startup_events);
    thread::Builder::new()
        .name("sairplay-libraop-log".into())
        .spawn(move || {
            let mut connected = false;
            let mut last_detail: Option<String> = None;
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let lower = line.to_ascii_lowercase();

                // Keep source errors while connecting so readiness failures
                // preserve the helper's actual RTSP/transport reason.
                if let Ok(mut events) = reader_events.lock() {
                    if !connected
                        || lower.contains("error")
                        || lower.contains("failed")
                    {
                        events.push(format!("{reader_name}: {line}"));
                    }
                }

                if !connected && lower.contains("connected to") {
                    connected = true;
                    let _ = connected_tx.send(Ok(()));
                } else if let Some((requested, actual)) = parse_started_status(&line) {
                    let _ = started_tx.send(Ok((requested, actual)));
                } else if let Some(head_unix_ms) = parse_flushed_status(&line) {
                    let _ = flushed_tx.send(Ok(head_unix_ms));
                } else if lower.contains("[status] error code=start_failed") {
                    let _ = started_tx.send(Err(line.clone()));
                } else if lower.contains("[status] error code=flush_failed") {
                    let _ = flushed_tx.send(Err(line.clone()));
                } else if lower.contains("cannot connect to airplay device")
                    || lower.contains("request failed")
                    || lower.contains("auth-setup failed")
                    || lower.contains("pair again")
                    || lower.contains("no session in response")
                    || lower.contains("missing a rtp port")
                {
                    last_detail = Some(line);
                }
            }
            if !connected {
                let detail = last_detail.unwrap_or_else(|| {
                    "helper exited before reporting raopcl_connect readiness".into()
                });
                let _ = connected_tx.send(Err(detail));
            }
        })
        .map_err(|error| LegacyGroupError::Spawn {
            name: config.name.clone(),
            error: format!("cannot spawn helper log reader: {error}"),
        })?;

    let (pcm_tx, pcm_rx) =
        mpsc::sync_channel::<[u8; PCM352_PACKET_BYTES]>(WRITER_QUEUE_PACKETS);
    let (writer_control_tx, writer_control_rx) =
        mpsc::sync_channel::<LegacyWriterCommand>(2);
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
                writer_control_rx,
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
        pid: child_pid,
        pcm_tx,
        writer_control_tx,
        connected_rx,
        started_rx: command_session.then_some(started_rx),
        flushed_rx: command_session.then_some(flushed_rx),
        command_pipe,
        writer,
        volume_control: LegacyVolumeControl::new(volume_path),
    })
}

fn legacy_writer_loop(
    name: String,
    child: &mut Child,
    mut stdin: ChildStdin,
    pcm_rx: Receiver<[u8; PCM352_PACKET_BYTES]>,
    control_rx: Receiver<LegacyWriterCommand>,
    running: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
    active_members: Arc<AtomicU64>,
) {
    let mut quiesced = false;

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

        // Music Assistant's stdin_quiesced() first prevents any later write
        // from interleaving, then waits until every byte already queued by the
        // parent has reached the OS pipe.  This control path is the equivalent
        // boundary for the synchronous Windows writer: callers MUST stop the
        // producer before requesting Quiesce; we drain the bounded Rust queue,
        // flush ChildStdin, acknowledge, then hold all later writes until
        // Resume. The helper can safely drain the Windows pipe during that
        // quiet window.
        match control_rx.try_recv() {
            Ok(LegacyWriterCommand::Quiesce(reply)) => {
                let result = (|| -> Result<(), String> {
                    loop {
                        match pcm_rx.try_recv() {
                            Ok(packet) => stdin
                                .write_all(&packet)
                                .map_err(|error| format!("PCM pipe failed while quiescing: {error}"))?,
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => break,
                        }
                    }
                    stdin
                        .flush()
                        .map_err(|error| format!("PCM pipe flush failed while quiescing: {error}"))?;
                    Ok(())
                })();
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
                quiesced = true;
                continue;
            }
            Ok(LegacyWriterCommand::Resume) => {
                quiesced = false;
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
            Err(mpsc::TryRecvError::Empty) => {}
        }

        if quiesced {
            match control_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(LegacyWriterCommand::Resume) => quiesced = false,
                Ok(LegacyWriterCommand::Quiesce(reply)) => {
                    let _ = reply.send(Ok(()));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
            continue;
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

    // cliraop's source loop exits after stdin EOF once its buffered RAOP audio
    // drains, then calls raopcl_disconnect()/raopcl_destroy(). Give that path
    // time to complete so receivers see a normal FLUSH/TEARDOWN. The session
    // watchdog above still guarantees a stuck helper cannot hang the app.
    let graceful_deadline = std::time::Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < graceful_deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }

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

#[allow(dead_code)]
fn quiesce_legacy_writer(
    control_tx: &SyncSender<LegacyWriterCommand>,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    control_tx
        .send(LegacyWriterCommand::Quiesce(reply_tx))
        .map_err(|error| format!("cannot request PCM quiesce: {error}"))?;
    reply_rx
        .recv_timeout(RAOP_FLUSH_ACK_TIMEOUT)
        .map_err(|error| {
            format!(
                "PCM writer did not quiesce within {} ms: {}",
                RAOP_FLUSH_ACK_TIMEOUT.as_millis(),
                error
            )
        })?
}

#[allow(dead_code)]
fn resume_legacy_writer(
    control_tx: &SyncSender<LegacyWriterCommand>,
) -> Result<(), String> {
    control_tx
        .send(LegacyWriterCommand::Resume)
        .map_err(|error| format!("cannot resume PCM writer: {error}"))
}

fn open_command_pipe_writer(pipe_name: &str) -> std::io::Result<File> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match OpenOptions::new().write(true).open(pipe_name) {
            Ok(file) => return Ok(file),
            Err(error) if std::time::Instant::now() < deadline => {
                // The helper creates the Windows named-pipe server before its
                // receiver RTSP connect. Retry only the local attach race.
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::WouldBlock
                ) {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

fn send_start_command(pipe: &mut File, start_unix_ms: u64) -> std::io::Result<()> {
    write!(
        pipe,
        "START_UNIX_MS={}\nACTION=START\n",
        start_unix_ms
    )?;
    pipe.flush()
}

#[allow(dead_code)]
fn send_flush_command(pipe: &mut File) -> std::io::Result<()> {
    pipe.write_all(b"ACTION=FLUSH\n")?;
    pipe.flush()
}

fn parse_started_status(line: &str) -> Option<(u64, u64)> {
    if !line.starts_with("[STATUS] started ") {
        return None;
    }
    let mut requested = None;
    let mut actual = None;
    for field in line.split_whitespace().skip(2) {
        if let Some(value) = field.strip_prefix("requested_unix_ms=") {
            requested = value.parse::<u64>().ok();
        } else if let Some(value) = field.strip_prefix("at_unix_ms=") {
            actual = value.parse::<u64>().ok();
        }
    }
    Some((requested?, actual?))
}

/// Parse MSA's accepted FLUSH acknowledgement. Legacy RAOP intentionally has
/// no head_unix_ms: pinned cliairplay returns 0 from warm_head_unix_ms() for
/// RAOP. The optional form is retained so the cross-transport controller can
/// later share one acknowledgement shape with native AirPlay 2.
fn parse_flushed_status(line: &str) -> Option<GroupFlushAck> {
    parse_group_flush_status(line)
}

fn current_unix_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))
}

fn kill_helper_tree(pid: u32) {
    // /T also terminates descendants created by the helper, /F guarantees a
    // blocked RTSP/stdin helper cannot keep SAirplay2 alive after Stop/Exit.
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .status();
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

fn unix_ms_to_ntp(ms: u64) -> u64 {
    ((ms / 1000) << 32) | ((((ms % 1000) as u128) << 32) / 1000) as u64
}

fn ms_to_ntp(ms: u64) -> u64 {
    ((ms as u128) << 32).div_ceil(1000) as u64
}

fn frames_to_ntp(frames: u32) -> u64 {
    ((frames as u128) << 32).div_ceil(44_100) as u64
}

fn ntp_delta_to_ms(delta: u64) -> u64 {
    (((delta as u128) * 1000) >> 32) as u64
}


#[cfg(test)]
mod cross_transport_tests {
    use super::*;

    #[test]
    fn parses_msa_started_ack_exactly() {
        assert_eq!(
            parse_started_status(
                "[STATUS] started requested_unix_ms=12345 at_unix_ms=12345"
            ),
            Some((12345, 12345))
        );
        assert_eq!(
            parse_started_status(
                "[STATUS] started requested_unix_ms=12345 at_unix_ms=12700"
            ),
            Some((12345, 12700))
        );
    }

    #[test]
    fn flush_and_stdin_drain_timeouts_match_current_music_assistant() {
        assert_eq!(RAOP_FLUSH_ACK_TIMEOUT, Duration::from_secs(2));
    }

    #[test]
    fn stdin_quiesce_timeout_matches_music_assistant() {
        assert_eq!(RAOP_FLUSH_ACK_TIMEOUT, Duration::from_secs(2));
    }

    #[test]
    fn parses_msa_raop_flush_ack_without_a_warm_head() {
        assert_eq!(
            parse_flushed_status("[STATUS] flushed"),
            Some(GroupFlushAck::no_head_constraint())
        );
        assert_eq!(
            parse_flushed_status("[STATUS] flushed head_unix_ms=12345"),
            Some(GroupFlushAck::with_head(12345))
        );
        assert_eq!(parse_flushed_status("[STATUS] started requested_unix_ms=1 at_unix_ms=1"), None);
        assert_eq!(RAOP_FLUSH_ACK_TIMEOUT, Duration::from_millis(2_000));
    }

    #[test]
    fn ignores_non_start_status_lines() {
        assert_eq!(parse_started_status("[STATUS] flushed"), None);
        assert_eq!(parse_started_status("connected to 10.0.0.1"), None);
    }

    #[test]
    fn unix_ms_fixed_point_matches_pinned_raop_session_shape() {
        let ntp = unix_ms_to_ntp(50_007);
        assert_eq!((ntp >> 32) * 1000 + (((ntp & 0xFFFF_FFFF) as u128 * 1000) >> 32) as u64, 50_006);
    }
}
