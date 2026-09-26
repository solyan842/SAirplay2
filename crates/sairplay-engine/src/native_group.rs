use crate::{
    NativeSession, NativeSessionConfig, NativeVolumeControl, PtpEngine, RetransmitStats,
    WindowsGroupAudioKind, WindowsMultiroomAudioError, WindowsMultiroomAudioWorker,
    WindowsMultiroomJoinHandle,
};
use std::fmt;
use std::sync::Arc;

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

pub struct NativeGroupSession {
    kind: NativeGroupKind,
    members: Vec<(String, NativeSession)>,
    audio_worker: Option<WindowsMultiroomAudioWorker>,
    shared_ptp: Option<Arc<PtpEngine>>,
    use_ptp: bool,
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

        Ok(Self {
            kind,
            members,
            audio_worker: Some(audio_worker),
            shared_ptp,
            use_ptp,
        })
    }

    pub fn kind(&self) -> NativeGroupKind {
        self.kind
    }

    fn build_join_handle(&self) -> Option<NativeGroupJoinHandle> {
        let audio = self.audio_worker.as_ref()?.join_handle();
        Some(NativeGroupJoinHandle {
            shared_ptp: self.shared_ptp.as_ref().map(Arc::clone),
            use_ptp: self.use_ptp,
            audio,
        })
    }

    /// User-driven membership changes remain MultiRoom-only. Stereo Pair
    /// membership is fixed in the UI, matching the existing product contract.
    pub fn join_handle(&self) -> Option<NativeGroupJoinHandle> {
        (self.kind == NativeGroupKind::MultiRoom)
            .then(|| self.build_join_handle())
            .flatten()
    }

    /// Recovery path used after an unexpected member transport loss.
    ///
    /// Pinned Music Assistant preserves group intent when one member drops:
    /// the surviving members keep playing and the failed receiver is allowed
    /// to re-enter through the normal late-join path. This handle is therefore
    /// available for both MultiRoom and a fixed Stereo Pair.
    pub fn recovery_join_handle(&self) -> Option<NativeGroupJoinHandle> {
        self.build_join_handle()
    }

    pub fn adopt_member(&mut self, name: String, session: NativeSession) {
        // MultiRoom uses this for user-driven live joins; Stereo Pair uses the
        // same late-join path only to restore a member that died unexpectedly.
        if let Some(index) = self.members.iter().position(|(member, _)| member == &name) {
            self.members.remove(index);
        }
        self.members.push((name, session));
    }

    /// Remove one failed transport without dissolving the rest of the group.
    ///
    /// This deliberately bypasses the user-facing Stereo Pair membership lock:
    /// the pair definition is preserved by the caller and the missing member is
    /// scheduled for bounded re-join, exactly like Music Assistant's native
    /// sync-group failure lifecycle.
    pub fn detach_failed_member(&mut self, name: &str) -> Result<bool, NativeGroupError> {
        let Some(index) = self.members.iter().position(|(member, _)| member == name) else {
            return Ok(false);
        };
        if let Some(audio) = self.audio_worker.as_ref() {
            audio
                .join_handle()
                .remove_target(name.to_owned())
                .map_err(NativeGroupError::Audio)?;
        }
        self.members.remove(index);
        Ok(true)
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

    pub fn feedback_running(&self) -> bool {
        !self.members.is_empty()
            && self
                .members
                .iter()
                .all(|(_, session)| session.feedback_running())
    }

    pub fn feedback_error(&self) -> Option<String> {
        self.members.iter().find_map(|(name, session)| {
            session
                .feedback_error()
                .map(|error| format!("{name}: {error}"))
        })
    }

    /// Return members whose native control worker has ended unexpectedly.
    /// The caller removes only those transports; surviving members keep the
    /// shared producer and PTP timeline alive.
    pub fn failed_feedback_members(&self) -> Vec<(String, String)> {
        self.members
            .iter()
            .filter(|(_, session)| !session.feedback_running())
            .map(|(name, session)| {
                (
                    name.clone(),
                    session
                        .feedback_error()
                        .unwrap_or_else(|| "feedback keepalive worker stopped".to_owned()),
                )
            })
            .collect()
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
