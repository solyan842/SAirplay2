#![cfg(windows)]

//! Windows PCM adapter for the concrete MSA-pinned RAOP session.
//!
//! RAOP is a first-class receiver lane and shares the same Windows PCM source
//! and bounded hub as native AP2. Transport ownership remains entirely inside
//! the pinned libraop-backed session; this worker only adapts WASAPI capture to
//! that transport and mirrors MSA's quiesce -> transport FLUSH -> local drain
//! ordering at lifecycle boundaries.

use crate::{
    Ap2AudioFormat, MsaRaopConfig, MsaRaopError, MsaRaopPcmWriter, MsaRaopSession,
    WasapiLoopbackError, WindowsPcmHub, WindowsPcmSource,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FLUSH_ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum WindowsRaopWorkerError {
    Session(MsaRaopError),
    Capture(WasapiLoopbackError),
    Worker(String),
}
impl fmt::Display for WindowsRaopWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(e) => write!(f, "{e}"),
            Self::Capture(e) => write!(f, "{e}"),
            Self::Worker(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for WindowsRaopWorkerError {}
impl From<MsaRaopError> for WindowsRaopWorkerError {
    fn from(v: MsaRaopError) -> Self { Self::Session(v) }
}
impl From<WasapiLoopbackError> for WindowsRaopWorkerError {
    fn from(v: WasapiLoopbackError) -> Self { Self::Capture(v) }
}

pub type SharedMsaRaopSession = Arc<Mutex<MsaRaopSession>>;

pub struct WindowsRaopAudioWorker {
    session: SharedMsaRaopSession,
    running: Arc<AtomicBool>,
    delivery_enabled: Arc<AtomicBool>,
    /// Mirrors cliairplay's g_audio_send_lock. Lifecycle commands take this
    /// gate before touching the transport so no packet can race a FLUSH/PAUSE.
    send_gate: Arc<Mutex<()>>,
    pcm_hub: WindowsPcmHub,
    pcm_source: WindowsPcmSource,
    first_start_done: Arc<AtomicBool>,
    writer_worker: Option<JoinHandle<()>>,
    health_worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
    startup_events: Arc<Mutex<Vec<String>>>,
}

impl WindowsRaopAudioWorker {
    pub fn connect(config: MsaRaopConfig) -> Result<Self, WindowsRaopWorkerError> {
        let session = Arc::new(Mutex::new(MsaRaopSession::connect(config)?));
        Self::start(session)
    }

    pub fn start(session: SharedMsaRaopSession) -> Result<Self, WindowsRaopWorkerError> {
        let (pcm_writer, ready) = {
            let guard = session
                .lock()
                .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            (guard.pcm_writer(), guard.ready())
        };
        let audio_format = Ap2AudioFormat {
            sample_rate: ready.sample_rate,
            bit_depth: ready.bit_depth,
            channels: ready.channels,
        };

        let running = Arc::new(AtomicBool::new(true));
        let delivery_enabled = Arc::new(AtomicBool::new(false));
        let send_gate = Arc::new(Mutex::new(()));
        let pcm_hub = WindowsPcmHub::new(audio_format);
        let first_start_done = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));
        let startup_events = Arc::new(Mutex::new(Vec::<String>::new()));

        // One transport-agnostic Windows producer for both AP1/RAOP and AP2.
        // It owns WASAPI capture and the bounded PCM hub; receiver network I/O
        // remains on the consumer below and can never block capture.
        let mut pcm_source = WindowsPcmSource::spawn(
            audio_format,
            pcm_hub.clone(),
            Arc::clone(&running),
            Arc::clone(&last_error),
            Arc::clone(&startup_events),
            None,
        )
        .map_err(|e| WindowsRaopWorkerError::Worker(e.to_string()))?;

        let running_w = Arc::clone(&running);
        let enabled_w = Arc::clone(&delivery_enabled);
        let gate_w = Arc::clone(&send_gate);
        let ring_w = pcm_hub.ring();
        let error_w = Arc::clone(&last_error);
        let writer_worker = match thread::Builder::new()
            .name("msa-raop-writer".into())
            .spawn(move || {
                while running_w.load(Ordering::SeqCst) {
                    if !enabled_w.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }

                    // The gate is the Windows equivalent of MSA's
                    // g_audio_send_lock: once a lifecycle command owns it, no
                    // old packet can enter libraop until that boundary ends.
                    let _gate = match gate_w.lock() {
                        Ok(v) => v,
                        Err(_) => {
                            if let Ok(mut slot) = error_w.lock() {
                                *slot = Some("RAOP send gate poisoned".into());
                            }
                            running_w.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    if !enabled_w.load(Ordering::SeqCst) {
                        continue;
                    }

                    let packet = match ring_w.lock() {
                        Ok(mut ring) => ring.pop_packet(),
                        Err(_) => {
                            if let Ok(mut slot) = error_w.lock() {
                                *slot = Some("RAOP PCM ring mutex poisoned".into());
                            }
                            running_w.store(false, Ordering::SeqCst);
                            break;
                        }
                    };

                    let Some(packet) = packet else {
                        drop(_gate);
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    };

                    if let Err(e) = pcm_writer.write_packet(&packet) {
                        if let Ok(mut slot) = error_w.lock() {
                            *slot = Some(e.to_string());
                        }
                        running_w.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                pcm_source.stop();
                return Err(WindowsRaopWorkerError::Worker(format!(
                    "spawn RAOP writer: {e}"
                )));
            }
        };

        let running_h = Arc::clone(&running);
        let session_h = Arc::clone(&session);
        let error_h = Arc::clone(&last_error);
        let health_worker = match thread::Builder::new()
            .name("msa-raop-health".into())
            .spawn(move || {
                while running_h.load(Ordering::SeqCst) {
                    let alive = session_h
                        .lock()
                        .map(|mut s| s.helper_alive())
                        .unwrap_or(false);
                    if !alive {
                        if let Ok(mut slot) = error_h.lock() {
                            *slot = Some("RAOP transport exited (control/media unhealthy)".into());
                        }
                        running_h.store(false, Ordering::SeqCst);
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }) {
            Ok(worker) => worker,
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                pcm_source.stop();
                let _ = writer_worker.join();
                return Err(WindowsRaopWorkerError::Worker(format!(
                    "spawn RAOP health monitor: {e}"
                )));
            }
        };

        match pcm_source.wait_ready(Duration::from_secs(3)) {
            Ok(()) => {
                if let Ok(mut events) = startup_events.lock() {
                    events.push(
                        "MSA INPUT RAOP first-class lane: shared WindowsPcmSource + WindowsPcmHub active; transport remains pinned libraop."
                            .into(),
                    );
                }
                Ok(Self {
                    session,
                    running,
                    delivery_enabled,
                    send_gate,
                    pcm_hub,
                    pcm_source,
                    first_start_done,
                    writer_worker: Some(writer_worker),
                    health_worker: Some(health_worker),
                    last_error,
                    startup_events,
                })
            }
            Err(e) => {
                running.store(false, Ordering::SeqCst);
                pcm_source.stop();
                let _ = writer_worker.join();
                let _ = health_worker.join();
                Err(WindowsRaopWorkerError::Worker(e.to_string()))
            }
        }
    }

    pub fn session(&self) -> SharedMsaRaopSession { Arc::clone(&self.session) }
    pub fn audio_ready(&self) -> bool { self.pcm_hub.audio_ready() }
    pub fn is_running(&self) -> bool { self.running.load(Ordering::SeqCst) }
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|v| v.clone())
    }
    pub fn discontinuities(&self) -> u64 { self.pcm_source.discontinuities() }
    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        self.pcm_source.last_discontinuity_frame()
    }
    pub fn startup_events(&self) -> Vec<String> {
        self.startup_events.lock().map(|v| v.clone()).unwrap_or_default()
    }
    pub fn drain_startup_events(&self) -> Vec<String> {
        self.startup_events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn commit_start(
        &self,
        requested_unix_ms: u64,
    ) -> Result<crate::timing::StartResolution, WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        let first = !self.first_start_done.load(Ordering::SeqCst);
        let start = if first {
            session.commit_start(requested_unix_ms)?
        } else {
            session.start_after_flush(requested_unix_ms)?
        };
        if first {
            // Same gate as cliairplay session_commit: metadata must land after
            // START commit but before captured PCM delivery opens.
            session.ensure_initial_metadata()?;
        }
        self.first_start_done.store(true, Ordering::SeqCst);
        self.delivery_enabled.store(true, Ordering::SeqCst);
        Ok(start)
    }

    pub fn flush_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;

        // Exact MSA boundary: quiesce sender first, FLUSH the receiver, then
        // clear/drain pre-boundary local PCM while sends remain quiesced.
        self.delivery_enabled.store(false, Ordering::SeqCst);
        {
            let mut session = self
                .session
                .lock()
                .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            session.flush()?;
        }

        let generation = self.pcm_hub.request_flush();
        let deadline = Instant::now() + FLUSH_ACK_TIMEOUT;
        while self.pcm_hub.flush_ack_generation() < generation {
            if !self.is_running() {
                return Err(WindowsRaopWorkerError::Worker(
                    "RAOP worker stopped during FLUSH".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(WindowsRaopWorkerError::Worker(
                    "RAOP FLUSH PCM barrier timed out".into(),
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    pub fn standby_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        self.delivery_enabled.store(false, Ordering::SeqCst);
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.standby()?;
        Ok(())
    }

    pub fn pause_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        self.delivery_enabled.store(false, Ordering::SeqCst);
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.pause()?;
        // No local flush-generation bump: new captured content remains in the
        // bounded persistent hub for ACTION=PLAY, matching pinned MSA pause.
        Ok(())
    }

    pub fn play_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.play()?;
        self.delivery_enabled.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn stop_content(&self) -> Result<(), WindowsRaopWorkerError> {
        let _gate = self
            .send_gate
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP send gate poisoned".into()))?;
        self.delivery_enabled.store(false, Ordering::SeqCst);
        let mut session = self
            .session
            .lock()
            .map_err(|_| WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.stop()?;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.delivery_enabled.store(false, Ordering::SeqCst);
        self.pcm_source.stop();
        if let Some(w) = self.writer_worker.take() { let _ = w.join(); }
        if let Some(w) = self.health_worker.take() { let _ = w.join(); }
        if let Ok(mut session) = self.session.lock() { session.disconnect(); }
    }
}

impl Drop for WindowsRaopAudioWorker {
    fn drop(&mut self) { self.stop(); }
}
