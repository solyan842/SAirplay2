#![cfg(windows)]

//! RAOP live-source lifecycle owner for Windows.
//!
//! The hardware-locked SOtM transport worker remains byte-for-byte preserved in
//! `windows_raop_worker_base.rs`. This layer owns only the lifecycle seam that a
//! live WASAPI source needs but a normal pre-buffered MSA stdin producer does
//! not: do not commit START before real PCM exists, and park/re-anchor a RAOP
//! stream when the local PCM source has been empty long enough for the audible
//! head to become infeasible. Native AP2 is untouched.

#[path = "windows_raop_worker_base.rs"]
mod base;

pub use base::{SharedMsaRaopSession, WindowsRaopWorkerError};

use crate::{MsaRaopConfig, MsaRaopSession, MsaRaopState};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Short capture/ring starvation is normal on the validated SOtM lane. Only a
// sustained empty source is eligible for an inferred lifecycle boundary.
const RAOP_SOURCE_EMPTY_MIN: Duration = Duration::from_millis(250);
// Keep the same sender-side feasibility floor used by the preserved worker.
// Once the remaining audible head reaches this floor, continuing the old START
// is no longer useful for a live source: FLUSH while the source is still empty,
// then wait for fresh PCM and let the base worker create START_AFTER_FLUSH.
const RAOP_REANCHOR_HEAD_FLOOR_MS: i128 = 200;
const RAOP_LIFECYCLE_POLL: Duration = Duration::from_millis(5);

fn unix_now_ms() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis().min(i128::MAX as u128) as i128)
        .unwrap_or(0)
}

fn head_ahead_ms(session: &SharedMsaRaopSession) -> Option<i128> {
    let head = session.lock().ok()?.head_audible_unix_ms();
    (head != 0).then(|| head as i128 - unix_now_ms())
}

fn should_park_live_raop(
    state: MsaRaopState,
    start_pending: bool,
    audio_ready: bool,
    empty_for: Duration,
    head_ahead: Option<i128>,
) -> bool {
    matches!(state, MsaRaopState::Streaming)
        && !start_pending
        && !audio_ready
        && empty_for >= RAOP_SOURCE_EMPTY_MIN
        && head_ahead
            .map(|ahead| ahead <= RAOP_REANCHOR_HEAD_FLOOR_MS)
            .unwrap_or(false)
}

fn set_lifecycle_error(
    slot: &Arc<Mutex<Option<String>>>,
    running: &Arc<AtomicBool>,
    message: String,
) {
    if let Ok(mut error) = slot.lock() {
        *error = Some(message);
    }
    running.store(false, Ordering::SeqCst);
}

/// Thin lifecycle wrapper around the hardware-locked RAOP transport worker.
///
/// The inner worker still owns WASAPI, the 4224-frame reservoir, libraop,
/// pacing, ALAC, NTP and every wire command. This wrapper only decides *when*
/// START/FLUSH boundaries are legal for a live Windows source.
pub struct WindowsRaopAudioWorker {
    inner: Arc<Mutex<base::WindowsRaopAudioWorker>>,
    session: SharedMsaRaopSession,
    lifecycle_running: Arc<AtomicBool>,
    pending_start: Arc<AtomicBool>,
    pending_requested_unix_ms: Arc<AtomicU64>,
    lifecycle_error: Arc<Mutex<Option<String>>>,
    lifecycle_events: Arc<Mutex<Vec<String>>>,
    lifecycle_worker: Option<JoinHandle<()>>,
}

impl WindowsRaopAudioWorker {
    pub fn connect(config: MsaRaopConfig) -> Result<Self, WindowsRaopWorkerError> {
        Self::wrap(base::WindowsRaopAudioWorker::connect(config)?)
    }

    pub fn start(session: SharedMsaRaopSession) -> Result<Self, WindowsRaopWorkerError> {
        Self::wrap(base::WindowsRaopAudioWorker::start(session)?)
    }

    fn wrap(inner: base::WindowsRaopAudioWorker) -> Result<Self, WindowsRaopWorkerError> {
        let session = inner.session();
        let inner = Arc::new(Mutex::new(inner));
        let lifecycle_running = Arc::new(AtomicBool::new(true));
        let pending_start = Arc::new(AtomicBool::new(false));
        let pending_requested_unix_ms = Arc::new(AtomicU64::new(0));
        let lifecycle_error = Arc::new(Mutex::new(None));
        let lifecycle_events = Arc::new(Mutex::new(Vec::<String>::new()));

        let inner_t = Arc::clone(&inner);
        let session_t = Arc::clone(&session);
        let running_t = Arc::clone(&lifecycle_running);
        let pending_t = Arc::clone(&pending_start);
        let requested_t = Arc::clone(&pending_requested_unix_ms);
        let error_t = Arc::clone(&lifecycle_error);
        let events_t = Arc::clone(&lifecycle_events);

        let lifecycle_worker = match thread::Builder::new()
            .name("msa-raop-lifecycle".into())
            .spawn(move || {
                let mut empty_since: Option<Instant> = None;

                while running_t.load(Ordering::SeqCst) {
                    let (inner_running, audio_ready) = match inner_t.lock() {
                        Ok(worker) => (worker.is_running(), worker.audio_ready()),
                        Err(_) => {
                            set_lifecycle_error(
                                &error_t,
                                &running_t,
                                "RAOP lifecycle inner-worker mutex poisoned".into(),
                            );
                            break;
                        }
                    };

                    if !inner_running {
                        running_t.store(false, Ordering::SeqCst);
                        break;
                    }

                    // Initial START and post-FLUSH resume are both deferred
                    // until the producer has observed real non-silent PCM.
                    // While delivery is still disabled the base ring can only
                    // grow, so audio_ready=true is a stable first-content edge.
                    if pending_t.load(Ordering::SeqCst) {
                        empty_since = None;
                        if audio_ready {
                            let state = match session_t.lock() {
                                Ok(session) => session.state(),
                                Err(_) => {
                                    set_lifecycle_error(
                                        &error_t,
                                        &running_t,
                                        "RAOP lifecycle session mutex poisoned".into(),
                                    );
                                    break;
                                }
                            };

                            if matches!(state, MsaRaopState::Connected | MsaRaopState::Flushed) {
                                // Re-check the pending bit after taking the
                                // transport owner: an explicit STOP/PAUSE may
                                // have superseded this inferred start.
                                let requested = requested_t.load(Ordering::SeqCst);
                                let start_result = match inner_t.lock() {
                                    Ok(worker) => {
                                        if pending_t.load(Ordering::SeqCst) {
                                            worker.commit_start(requested).map(Some)
                                        } else {
                                            Ok(None)
                                        }
                                    }
                                    Err(_) => Err(WindowsRaopWorkerError::Worker(
                                        "RAOP lifecycle inner-worker mutex poisoned".into(),
                                    )),
                                };

                                match start_result {
                                    Ok(Some(start)) => {
                                        pending_t.store(false, Ordering::SeqCst);
                                        requested_t.store(0, Ordering::SeqCst);
                                        if let Ok(mut events) = events_t.lock() {
                                            events.push(format!(
                                                "MSA RAOP LIVE START committed on PCM edge: requested={} accepted={}; preserved worker owns reservoir/pacing.",
                                                requested,
                                                start.at_unix_ms,
                                            ));
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        set_lifecycle_error(
                                            &error_t,
                                            &running_t,
                                            format!("RAOP deferred START failed: {error}"),
                                        );
                                        break;
                                    }
                                }
                            } else if matches!(
                                state,
                                MsaRaopState::Paused | MsaRaopState::Stopped | MsaRaopState::Down
                            ) {
                                pending_t.store(false, Ordering::SeqCst);
                                requested_t.store(0, Ordering::SeqCst);
                            } else if matches!(state, MsaRaopState::Streaming) {
                                // Another explicit command already committed a
                                // valid START; do not manufacture a second one.
                                pending_t.store(false, Ordering::SeqCst);
                                requested_t.store(0, Ordering::SeqCst);
                            }
                        }

                        thread::sleep(RAOP_LIFECYCLE_POLL);
                        continue;
                    }

                    let state = match session_t.lock() {
                        Ok(session) => session.state(),
                        Err(_) => {
                            set_lifecycle_error(
                                &error_t,
                                &running_t,
                                "RAOP lifecycle session mutex poisoned".into(),
                            );
                            break;
                        }
                    };

                    if matches!(state, MsaRaopState::Streaming) && !audio_ready {
                        let started = *empty_since.get_or_insert_with(Instant::now);
                        let empty_for = started.elapsed();
                        let head_before = head_ahead_ms(&session_t);

                        if should_park_live_raop(
                            state,
                            false,
                            false,
                            empty_for,
                            head_before,
                        ) {
                            // The source may resume exactly as the threshold is
                            // crossed. Re-check both the ring-ready edge and the
                            // transport state while owning the base worker. If
                            // fresh PCM already exists, leave the live stream
                            // alone so the beginning of the new song is never
                            // flushed after arrival.
                            let park_result = match inner_t.lock() {
                                Ok(worker) => {
                                    let state_now = session_t
                                        .lock()
                                        .map(|session| session.state())
                                        .unwrap_or(MsaRaopState::Down);
                                    let head_now = head_ahead_ms(&session_t);
                                    if !pending_t.load(Ordering::SeqCst)
                                        && !worker.audio_ready()
                                        && matches!(state_now, MsaRaopState::Streaming)
                                        && head_now
                                            .map(|ahead| {
                                                ahead <= RAOP_REANCHOR_HEAD_FLOOR_MS
                                            })
                                            .unwrap_or(false)
                                    {
                                        worker.flush_content().map(|_| true)
                                    } else {
                                        Ok(false)
                                    }
                                }
                                Err(_) => Err(WindowsRaopWorkerError::Worker(
                                    "RAOP lifecycle inner-worker mutex poisoned".into(),
                                )),
                            };

                            match park_result {
                                Ok(true) => {
                                    // FLUSH already quiesced sender -> flushed
                                    // receiver -> drained/reset local PCM in the
                                    // preserved worker. Keep delivery closed and
                                    // wait for fresh non-silent PCM before a
                                    // START_AFTER_FLUSH with a fresh live floor.
                                    requested_t.store(0, Ordering::SeqCst);
                                    pending_t.store(true, Ordering::SeqCst);
                                    empty_since = None;
                                    if let Ok(mut events) = events_t.lock() {
                                        events.push(format!(
                                            "MSA RAOP LIVE SOURCE parked before stale timeline: empty={}ms head_ahead_ms={:?}; FLUSH complete, waiting for fresh PCM before START_AFTER_FLUSH.",
                                            empty_for.as_millis(),
                                            head_before,
                                        ));
                                    }
                                }
                                Ok(false) => {
                                    empty_since = None;
                                }
                                Err(error) => {
                                    set_lifecycle_error(
                                        &error_t,
                                        &running_t,
                                        format!("RAOP source-idle FLUSH failed: {error}"),
                                    );
                                    break;
                                }
                            }
                        }
                    } else {
                        empty_since = None;
                    }

                    thread::sleep(RAOP_LIFECYCLE_POLL);
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                lifecycle_running.store(false, Ordering::SeqCst);
                if let Ok(mut worker) = inner.lock() {
                    worker.stop();
                }
                return Err(WindowsRaopWorkerError::Worker(format!(
                    "spawn RAOP lifecycle monitor: {error}"
                )));
            }
        };

        if let Ok(mut events) = lifecycle_events.lock() {
            events.push(
                "MSA RAOP LIVE lifecycle active: START waits for non-silent WASAPI PCM; stale source-idle timelines FLUSH before fresh resume; native AP2 untouched."
                    .into(),
            );
        }

        Ok(Self {
            inner,
            session,
            lifecycle_running,
            pending_start,
            pending_requested_unix_ms,
            lifecycle_error,
            lifecycle_events,
            lifecycle_worker: Some(lifecycle_worker),
        })
    }

    pub fn session(&self) -> SharedMsaRaopSession {
        Arc::clone(&self.session)
    }

    pub fn audio_ready(&self) -> bool {
        self.inner
            .lock()
            .map(|worker| worker.audio_ready())
            .unwrap_or(false)
    }

    pub fn is_running(&self) -> bool {
        if !self.lifecycle_running.load(Ordering::SeqCst) {
            return false;
        }
        if self
            .lifecycle_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
            .is_some()
        {
            return false;
        }
        self.inner
            .lock()
            .map(|worker| worker.is_running())
            .unwrap_or(false)
    }

    pub fn last_error(&self) -> Option<String> {
        if let Some(error) = self
            .lifecycle_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
        {
            return Some(error);
        }
        self.inner
            .lock()
            .ok()
            .and_then(|worker| worker.last_error())
    }

    pub fn discontinuities(&self) -> u64 {
        self.inner
            .lock()
            .map(|worker| worker.discontinuities())
            .unwrap_or(0)
    }

    pub fn last_discontinuity_frame(&self) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|worker| worker.last_discontinuity_frame())
    }

    pub fn startup_events(&self) -> Vec<String> {
        let mut events = self
            .inner
            .lock()
            .map(|worker| worker.startup_events())
            .unwrap_or_default();
        events.extend(
            self.lifecycle_events
                .lock()
                .map(|events| events.clone())
                .unwrap_or_default(),
        );
        events
    }

    pub fn drain_startup_events(&self) -> Vec<String> {
        let mut events = self
            .inner
            .lock()
            .map(|worker| worker.drain_startup_events())
            .unwrap_or_default();
        if let Ok(mut lifecycle) = self.lifecycle_events.lock() {
            events.extend(lifecycle.drain(..));
        }
        events
    }

    /// For a live Windows source, START is legal only once real PCM exists.
    /// If audio is already buffered, keep the preserved synchronous path. If
    /// not, return the caller's planning anchor and let the lifecycle monitor
    /// commit the actual transport START on the first PCM edge. The real
    /// accepted anchor is emitted in startup diagnostics.
    pub fn commit_start(
        &self,
        requested_unix_ms: u64,
    ) -> Result<crate::timing::StartResolution, WindowsRaopWorkerError> {
        if !self.is_running() {
            return Err(WindowsRaopWorkerError::Worker(
                "RAOP lifecycle worker is not running".into(),
            ));
        }

        if self.audio_ready() {
            return self
                .inner
                .lock()
                .map_err(|_| {
                    WindowsRaopWorkerError::Worker(
                        "RAOP lifecycle inner-worker mutex poisoned".into(),
                    )
                })?
                .commit_start(requested_unix_ms);
        }

        let state = self
            .session
            .lock()
            .map_err(|_| {
                WindowsRaopWorkerError::Worker("RAOP lifecycle session mutex poisoned".into())
            })?
            .state();
        if !matches!(state, MsaRaopState::Connected | MsaRaopState::Flushed) {
            return Err(WindowsRaopWorkerError::Worker(format!(
                "deferred RAOP START requires Connected/Flushed state, got {state:?}"
            )));
        }

        self.pending_requested_unix_ms
            .store(requested_unix_ms, Ordering::SeqCst);
        self.pending_start.store(true, Ordering::SeqCst);
        if let Ok(mut events) = self.lifecycle_events.lock() {
            events.push(format!(
                "MSA RAOP LIVE START armed: requested={requested_unix_ms}; waiting for first non-silent WASAPI PCM before transport START."
            ));
        }

        // The public StartResolution predates deferred RAOP and has no Pending
        // variant. Preserve the caller's planning anchor here; the actual
        // transport-accepted instant is logged when the monitor commits START.
        Ok(crate::timing::StartResolution {
            requested_unix_ms,
            at_unix_ms: requested_unix_ms,
            corrected_forward: false,
        })
    }

    pub fn start_pending(&self) -> bool {
        self.pending_start.load(Ordering::SeqCst)
    }

    pub fn flush_content(&self) -> Result<(), WindowsRaopWorkerError> {
        self.pending_start.store(false, Ordering::SeqCst);
        self.pending_requested_unix_ms.store(0, Ordering::SeqCst);
        self.inner
            .lock()
            .map_err(|_| {
                WindowsRaopWorkerError::Worker("RAOP lifecycle inner-worker mutex poisoned".into())
            })?
            .flush_content()
    }

    pub fn standby_content(&self) -> Result<(), WindowsRaopWorkerError> {
        self.pending_start.store(false, Ordering::SeqCst);
        self.pending_requested_unix_ms.store(0, Ordering::SeqCst);
        self.inner
            .lock()
            .map_err(|_| {
                WindowsRaopWorkerError::Worker("RAOP lifecycle inner-worker mutex poisoned".into())
            })?
            .standby_content()
    }

    pub fn pause_content(&self) -> Result<(), WindowsRaopWorkerError> {
        self.pending_start.store(false, Ordering::SeqCst);
        self.pending_requested_unix_ms.store(0, Ordering::SeqCst);
        self.inner
            .lock()
            .map_err(|_| {
                WindowsRaopWorkerError::Worker("RAOP lifecycle inner-worker mutex poisoned".into())
            })?
            .pause_content()
    }

    pub fn play_content(&self) -> Result<(), WindowsRaopWorkerError> {
        self.inner
            .lock()
            .map_err(|_| {
                WindowsRaopWorkerError::Worker("RAOP lifecycle inner-worker mutex poisoned".into())
            })?
            .play_content()
    }

    pub fn stop_content(&self) -> Result<(), WindowsRaopWorkerError> {
        self.pending_start.store(false, Ordering::SeqCst);
        self.pending_requested_unix_ms.store(0, Ordering::SeqCst);
        self.inner
            .lock()
            .map_err(|_| {
                WindowsRaopWorkerError::Worker("RAOP lifecycle inner-worker mutex poisoned".into())
            })?
            .stop_content()
    }

    pub fn stop(&mut self) {
        self.lifecycle_running.store(false, Ordering::SeqCst);
        self.pending_start.store(false, Ordering::SeqCst);
        self.pending_requested_unix_ms.store(0, Ordering::SeqCst);
        if let Some(worker) = self.lifecycle_worker.take() {
            let _ = worker.join();
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.stop();
        }
    }
}

impl Drop for WindowsRaopAudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_empty_burst_does_not_create_lifecycle_boundary() {
        assert!(!should_park_live_raop(
            MsaRaopState::Streaming,
            false,
            false,
            Duration::from_millis(40),
            Some(100),
        ));
    }

    #[test]
    fn warm_head_does_not_park_even_when_source_is_empty() {
        assert!(!should_park_live_raop(
            MsaRaopState::Streaming,
            false,
            false,
            Duration::from_millis(500),
            Some(700),
        ));
    }

    #[test]
    fn infeasible_head_parks_only_while_source_is_still_empty() {
        assert!(should_park_live_raop(
            MsaRaopState::Streaming,
            false,
            false,
            Duration::from_millis(500),
            Some(150),
        ));
        assert!(!should_park_live_raop(
            MsaRaopState::Streaming,
            false,
            true,
            Duration::from_millis(500),
            Some(150),
        ));
    }

    #[test]
    fn pending_or_non_streaming_session_never_auto_parks() {
        assert!(!should_park_live_raop(
            MsaRaopState::Streaming,
            true,
            false,
            Duration::from_secs(5),
            Some(-5_000),
        ));
        assert!(!should_park_live_raop(
            MsaRaopState::Connected,
            false,
            false,
            Duration::from_secs(5),
            Some(-5_000),
        ));
    }
}
