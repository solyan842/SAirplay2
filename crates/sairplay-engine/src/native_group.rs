use crate::{
    NativeSession, NativeSessionConfig, NativeVolumeControl, PtpEngine, RetransmitStats,
    WindowsGroupAudioKind, WindowsMultiroomAudioError, WindowsMultiroomAudioWorker,
    WindowsMultiroomJoinHandle,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct NativeGroupMemberConfig {
    pub name: String,
    pub config: NativeSessionConfig,
}

impl NativeGroupMemberConfig {
    pub fn new(name: impl Into<String>, config: NativeSessionConfig) -> Self {
        Self {
            name: name.into(),
            config,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeGroupKind {
    StereoPair,
    MultiRoom,
}

#[derive(Debug)]
pub enum NativeGroupError {
    EmptyGroup,
    InvalidMembership { kind: NativeGroupKind, members: usize },
    MembershipLocked,
    Member { name: String, error: String },
    Audio(WindowsMultiroomAudioError),
}

impl fmt::Display for NativeGroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup => write!(f, "AirPlay session requires at least one receiver"),
            Self::InvalidMembership { kind, members } => write!(f, "{kind:?} has invalid receiver count: {members}"),
            Self::MembershipLocked => write!(f, "Stereo Pair membership is fixed for the session"),
            Self::Member { name, error } => write!(f, "{name}: {error}"),
            Self::Audio(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NativeGroupError {}

#[derive(Clone)]
pub struct NativeGroupJoinHandle {
    shared_ptp: Option<Arc<PtpEngine>>,
    use_ptp: bool,
    audio: WindowsMultiroomJoinHandle,
}

impl NativeGroupJoinHandle {
    pub fn connect_member(
        &self,
        name: impl Into<String>,
        mut config: NativeSessionConfig,
    ) -> Result<(String, NativeSession), NativeGroupError> {
        let name = name.into();

        // Timing mode is a session-wide decision upstream. A late joiner never
        // introduces a second PTP daemon or changes an existing NTP group.
        let result = if self.use_ptp && config.supports_ptp {
            if let Some(engine) = self.shared_ptp.as_ref() {
                NativeSession::connect_with_shared_ptp(&config, Some(Arc::clone(engine)))
            } else {
                config.supports_ptp = false;
                config.follow_receiver_clock = false;
                NativeSession::connect(&config)
            }
        } else {
            config.supports_ptp = false;
            config.follow_receiver_clock = false;
            NativeSession::connect(&config)
        };

        let mut session = result.map_err(|error| NativeGroupError::Member {
            name: name.clone(),
            error: error.to_string(),
        })?;
        let target = session
            .take_windows_audio_target(name.clone())
            .map_err(|error| NativeGroupError::Member {
                name: name.clone(),
                error: error.to_string(),
            })?;
        self.audio
            .add_target(target)
            .map_err(NativeGroupError::Audio)?;
        Ok((name, session))
    }

    pub fn remove_audio_member(&self, name: impl Into<String>) -> Result<(), NativeGroupError> {
        self.audio
            .remove_target(name.into())
            .map_err(NativeGroupError::Audio)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MultiRoomFormatPlan {
    session_sample_rate: u32,
}

fn plan_multiroom_format(configs: &[NativeGroupMemberConfig]) -> MultiRoomFormatPlan {
    // Music Assistant selects ONE shared flow sample-rate from the intersection
    // of every output player's supported rates, but keeps bit depth per player.
    //
    // AirPlay's non-hires baseline is 44.1/16. A hi-res-enabled AirPlay 2
    // player contributes 44.1/24 and 48/24. Therefore:
    //   * if every member is hi-res enabled, 48 kHz is available to the group;
    //   * if any member is baseline 16-bit, the common rate is 44.1 kHz.
    //
    // Per-member /info remains authoritative during NativeSession::connect:
    // a hi-res member on a mixed group negotiates 44.1/24 while the baseline
    // member negotiates 44.1/16. The worker then performs depth-only handoff.
    let all_hires = !configs.is_empty()
        && configs.iter().all(|member| member.config.hires_enabled);
    MultiRoomFormatPlan {
        session_sample_rate: if all_hires { 48_000 } else { 44_100 },
    }
}

const AIRPLAY_REJOIN_ATTEMPT_DELAYS_SECS: [u64; 5] = [5, 15, 30, 60, 120];

struct GroupRecoveryResult {
    name: String,
    result: Result<NativeSession, String>,
}

pub struct NativeGroupSession {
    kind: NativeGroupKind,
    members: Vec<(String, NativeSession)>,
    audio_worker: Option<WindowsMultiroomAudioWorker>,
    shared_ptp: Option<Arc<PtpEngine>>,
    use_ptp: bool,
    member_configs: BTreeMap<String, NativeSessionConfig>,
    recovery_pending: BTreeSet<String>,
    recovery_tx: Sender<GroupRecoveryResult>,
    recovery_rx: Receiver<GroupRecoveryResult>,
    recovery_stop: Arc<AtomicBool>,
}

impl NativeGroupSession {
    pub fn connect(kind: NativeGroupKind, mut configs: Vec<NativeGroupMemberConfig>) -> Result<Self, NativeGroupError> {
        if configs.is_empty() {
            return Err(NativeGroupError::EmptyGroup);
        }
        match kind {
            NativeGroupKind::StereoPair if configs.len() != 2 => return Err(NativeGroupError::InvalidMembership { kind, members: configs.len() }),
            NativeGroupKind::MultiRoom if configs.len() < 2 => return Err(NativeGroupError::InvalidMembership { kind, members: configs.len() }),
            _ => {}
        }

        if kind == NativeGroupKind::MultiRoom {
            let plan = plan_multiroom_format(&configs);
            for member in &mut configs {
                member.config.session_sample_rate = plan.session_sample_rate;
            }
        }

        let member_configs = configs
            .iter()
            .map(|member| (member.name.clone(), member.config.clone()))
            .collect::<BTreeMap<_, _>>();

        // Source architecture: one shared PTP daemon owns 319/320 for every
        // native AirPlay 2 member. A one-member session uses the same object so
        // later joiners can be added without restarting playback.
        if let Some(index) = configs.iter().position(|member| member.config.supports_ptp) {
            configs.swap(0, index);
        }

        let group_wants_ptp = configs.iter().any(|member| member.config.supports_ptp);
        let mut shared_ptp: Option<Arc<PtpEngine>> = None;
        let mut members = Vec::<(String, NativeSession)>::with_capacity(configs.len());

        for (index, mut member) in configs.into_iter().enumerate() {
            let session_result = if index == 0 {
                NativeSession::connect(&member.config)
            } else if member.config.supports_ptp {
                if let Some(engine) = shared_ptp.as_ref() {
                    NativeSession::connect_with_shared_ptp(
                        &member.config,
                        Some(Arc::clone(engine)),
                    )
                } else {
                    member.config.supports_ptp = false;
                    member.config.follow_receiver_clock = false;
                    NativeSession::connect(&member.config)
                }
            } else {
                NativeSession::connect(&member.config)
            };

            let session = session_result.map_err(|error| NativeGroupError::Member {
                name: member.name.clone(),
                error: error.to_string(),
            })?;

            if index == 0 && group_wants_ptp {
                shared_ptp = session.shared_ptp_engine();
            }
            members.push((member.name, session));
        }

        let use_ptp = shared_ptp.is_some();
        let mut targets = Vec::with_capacity(members.len());
        for (name, session) in &mut members {
            let target = session
                .take_windows_audio_target(name.clone())
                .map_err(|error| NativeGroupError::Member {
                    name: name.clone(),
                    error: error.to_string(),
                })?;
            targets.push(target);
        }

        let worker_kind = match kind {
            NativeGroupKind::StereoPair => WindowsGroupAudioKind::StereoPair,
            NativeGroupKind::MultiRoom => WindowsGroupAudioKind::MultiRoom,
        };
        let audio_worker =
            WindowsMultiroomAudioWorker::start(worker_kind, targets).map_err(NativeGroupError::Audio)?;

        let (recovery_tx, recovery_rx) = mpsc::channel();
        Ok(Self {
            kind,
            members,
            audio_worker: Some(audio_worker),
            shared_ptp,
            use_ptp,
            member_configs,
            recovery_pending: BTreeSet::new(),
            recovery_tx,
            recovery_rx,
            recovery_stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn kind(&self) -> NativeGroupKind {
        self.kind
    }

    pub fn join_handle(&self) -> Option<NativeGroupJoinHandle> {
        if self.kind != NativeGroupKind::MultiRoom {
            return None;
        }
        let audio = self.audio_worker.as_ref()?.join_handle();
        Some(NativeGroupJoinHandle {
            shared_ptp: self.shared_ptp.as_ref().map(Arc::clone),
            use_ptp: self.use_ptp,
            audio,
        })
    }

    fn recovery_handle(&self) -> Option<NativeGroupJoinHandle> {
        let audio = self.audio_worker.as_ref()?.join_handle();
        Some(NativeGroupJoinHandle {
            shared_ptp: self.shared_ptp.as_ref().map(Arc::clone),
            use_ptp: self.use_ptp,
            audio,
        })
    }

    pub fn adopt_member(&mut self, name: String, session: NativeSession) {
        debug_assert_eq!(self.kind, NativeGroupKind::MultiRoom);
        if let Some(index) = self.members.iter().position(|(member, _)| member == &name) {
            self.members.remove(index);
        }
        self.members.push((name, session));
    }

    pub fn remove_member(&mut self, name: &str) -> Result<(), NativeGroupError> {
        if self.kind != NativeGroupKind::MultiRoom {
            return Err(NativeGroupError::MembershipLocked);
        }
        if let Some(audio) = self.audio_worker.as_ref() {
            audio
                .join_handle()
                .remove_target(name.to_owned())
                .map_err(NativeGroupError::Audio)?;
        }
        if let Some(index) = self.members.iter().position(|(member, _)| member == name) {
            self.members.remove(index);
        }
        Ok(())
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    pub fn audio_format(&self) -> Option<crate::Ap2AudioFormat> {
        let first = self.members.first()?.1.audio_format();
        self.members
            .iter()
            .all(|(_, session)| session.audio_format() == first)
            .then_some(first)
    }

    pub fn active_audio_members(&self) -> usize {
        self.audio_worker
            .as_ref()
            .map(WindowsMultiroomAudioWorker::active_members)
            .unwrap_or(0)
    }

    pub fn audio_running(&self) -> bool {
        self.audio_worker
            .as_ref()
            .is_some_and(WindowsMultiroomAudioWorker::is_running)
    }

    pub fn audio_error(&self) -> Option<String> {
        self.audio_worker
            .as_ref()
            .and_then(WindowsMultiroomAudioWorker::last_error)
    }

    pub fn audio_discontinuities(&self) -> u64 {
        self.audio_worker
            .as_ref()
            .map(WindowsMultiroomAudioWorker::discontinuity_count)
            .unwrap_or(0)
    }

    pub fn audio_last_discontinuity_frame(&self) -> Option<u64> {
        self.audio_worker
            .as_ref()
            .and_then(WindowsMultiroomAudioWorker::last_discontinuity_frame)
    }

    pub fn audio_first_non_silent_frame(&self) -> Option<u64> {
        self.audio_worker
            .as_ref()
            .and_then(WindowsMultiroomAudioWorker::first_non_silent_frame)
    }

    pub fn drain_startup_events(&self) -> Vec<String> {
        self.audio_worker
            .as_ref()
            .map(WindowsMultiroomAudioWorker::drain_startup_events)
            .unwrap_or_default()
    }

    /// Remove only an unexpectedly dead transport from the live fan-out and
    /// schedule the same bounded 5/15/30/60/120 s re-join ladder used by MSA.
    ///
    /// The surviving members keep their shared PTP timeline and continue
    /// playing. Re-joining goes through the normal late-join path, so the
    /// recovered member maps onto the live ring instead of restarting the group.
    pub fn poll_transport_recovery(&mut self) -> Vec<String> {
        let mut events = Vec::new();

        while let Ok(recovery) = self.recovery_rx.try_recv() {
            self.recovery_pending.remove(&recovery.name);
            match recovery.result {
                Ok(session) => {
                    if let Some(index) = self.members.iter().position(|(name, _)| name == &recovery.name) {
                        self.members.remove(index);
                    }
                    self.members.push((recovery.name.clone(), session));
                    events.push(format!(
                        "{}: automatic AirPlay group re-join succeeded.",
                        recovery.name
                    ));
                }
                Err(error) => {
                    events.push(format!(
                        "{}: automatic AirPlay group re-join exhausted: {}.",
                        recovery.name, error
                    ));
                }
            }
        }

        let failed = self
            .members
            .iter()
            .filter(|(name, session)| {
                !self.recovery_pending.contains(name) && !session.feedback_running()
            })
            .map(|(name, session)| {
                (
                    name.clone(),
                    session
                        .feedback_error()
                        .unwrap_or_else(|| "feedback worker stopped".to_owned()),
                )
            })
            .collect::<Vec<_>>();

        for (name, reason) in failed {
            // Preserve at least one live target. If every member is gone there
            // is no session left for a late join to heal, matching MSA's
            // whole-group terminal case.
            if self.active_audio_members() <= 1 {
                continue;
            }

            let Some(config) = self.member_configs.get(&name).cloned() else {
                continue;
            };
            let Some(handle) = self.recovery_handle() else {
                continue;
            };

            if handle.remove_audio_member(name.clone()).is_err() {
                continue;
            }
            if let Some(index) = self.members.iter().position(|(member, _)| member == &name) {
                self.members.remove(index);
            }
            self.recovery_pending.insert(name.clone());
            events.push(format!(
                "{}: removed from live AirPlay group after unexpected transport loss ({reason}); bounded re-join scheduled.",
                name
            ));

            let tx = self.recovery_tx.clone();
            let stop = Arc::clone(&self.recovery_stop);
            thread::Builder::new()
                .name(format!("sairplay-rejoin-{name}"))
                .spawn(move || {
                    let mut last_error = String::from("no re-join attempt completed");
                    for delay_secs in AIRPLAY_REJOIN_ATTEMPT_DELAYS_SECS {
                        // Sleep in small slices so explicit teardown cancels the
                        // ladder promptly instead of leaving a detached retry.
                        for _ in 0..delay_secs.saturating_mul(10) {
                            if stop.load(Ordering::SeqCst) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }

                        match handle.connect_member(name.clone(), config.clone()) {
                            Ok((_, session)) => {
                                let _ = tx.send(GroupRecoveryResult {
                                    name,
                                    result: Ok(session),
                                });
                                return;
                            }
                            Err(error) => {
                                last_error = error.to_string();
                            }
                        }
                    }
                    let _ = tx.send(GroupRecoveryResult {
                        name,
                        result: Err(last_error),
                    });
                })
                .ok();
        }

        events
    }

    pub fn feedback_running(&self) -> bool {
        self.members.iter().all(|(_, session)| session.feedback_running())
    }

    pub fn feedback_error(&self) -> Option<String> {
        self.members.iter().find_map(|(name, session)| {
            session
                .feedback_error()
                .map(|error| format!("{name}: {error}"))
        })
    }

    pub fn retransmit_stats(&self) -> RetransmitStats {
        self.members.iter().fold(RetransmitStats::default(), |mut total, (_, session)| {
            let stats = session.retransmit_stats();
            total.requested = total.requested.saturating_add(stats.requested);
            total.answered = total.answered.saturating_add(stats.answered);
            total.expired = total.expired.saturating_add(stats.expired);
            total.requested_over_1472 = total
                .requested_over_1472
                .saturating_add(stats.requested_over_1472);
            total.max_requested_wire_len =
                total.max_requested_wire_len.max(stats.max_requested_wire_len);
            total
        })
    }

    /// Per-member retransmit counters for runtime diagnostics.
    ///
    /// The aggregate above remains the session health surface.  This view is
    /// intentionally observational only so Stereo Pair/MultiRoom transport
    /// behavior is unchanged while logs can identify the receiver requesting
    /// each retransmission.
    pub fn member_retransmit_stats(&self) -> Vec<(String, RetransmitStats)> {
        self.members
            .iter()
            .map(|(name, session)| (name.clone(), session.retransmit_stats()))
            .collect()
    }

    pub fn volume_controls(&self) -> Vec<NativeVolumeControl> {
        self.members
            .iter()
            .map(|(_, session)| session.volume_control())
            .collect()
    }

    pub fn initial_volume_results(&self) -> Vec<(String, crate::VolumeSetResult)> {
        self.members
            .iter()
            .filter_map(|(name, session)| {
                session
                    .initial_volume_result()
                    .map(|result| (name.clone(), result))
            })
            .collect()
    }

    pub fn stop_audio(&mut self) {
        if let Some(mut worker) = self.audio_worker.take() {
            worker.stop();
        }
    }
}

impl Drop for NativeGroupSession {
    fn drop(&mut self) {
        self.recovery_stop.store(true, Ordering::SeqCst);
        // Keep the shared producer alive until every receiver has received its
        // TEARDOWN. This preserves the same clean shutdown boundary as MSA for
        // Stereo Pair and MultiRoom instead of starving all armed queues first.
        for (_, session) in &mut self.members {
            session.teardown_while_audio_hot();
        }
        self.stop_audio();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, hires_enabled: bool, requested_rate: u32) -> NativeGroupMemberConfig {
        let mut config = NativeSessionConfig::new("127.0.0.1", 7000);
        config.hires_enabled = hires_enabled;
        config.session_sample_rate = requested_rate;
        NativeGroupMemberConfig::new(name, config)
    }

    #[test]
    fn multiroom_plan_keeps_48k_when_every_member_is_hires() {
        let configs = vec![
            member("a", true, 48_000),
            member("b", true, 48_000),
        ];
        assert_eq!(plan_multiroom_format(&configs).session_sample_rate, 48_000);
    }

    #[test]
    fn multiroom_plan_mixed_24_and_16_uses_common_44100_rate() {
        let configs = vec![
            member("hires", true, 48_000),
            member("baseline", false, 44_100),
        ];
        assert_eq!(plan_multiroom_format(&configs).session_sample_rate, 44_100);
    }

    #[test]
    fn multiroom_plan_all_16bit_uses_44100_rate() {
        let configs = vec![
            member("a", false, 44_100),
            member("b", false, 44_100),
        ];
        assert_eq!(plan_multiroom_format(&configs).session_sample_rate, 44_100);
    }

    #[test]
    fn multiroom_plan_ignores_per_member_requested_rate_in_favor_of_common_rate() {
        let configs = vec![
            member("hires", true, 48_000),
            member("baseline", false, 48_000),
        ];
        assert_eq!(plan_multiroom_format(&configs).session_sample_rate, 44_100);
    }
}
