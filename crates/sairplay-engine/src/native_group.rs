use crate::{
    NativeSession, NativeSessionConfig, NativeVolumeControl, PtpEngine, RetransmitStats,
    WindowsMultiroomAudioError, WindowsMultiroomAudioWorker, WindowsMultiroomJoinHandle,
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

#[derive(Debug)]
pub enum NativeGroupError {
    EmptyGroup,
    Member { name: String, error: String },
    Audio(WindowsMultiroomAudioError),
}

impl fmt::Display for NativeGroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGroup => write!(f, "AirPlay session requires at least one receiver"),
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

pub struct NativeGroupSession {
    members: Vec<(String, NativeSession)>,
    audio_worker: Option<WindowsMultiroomAudioWorker>,
    shared_ptp: Option<Arc<PtpEngine>>,
    use_ptp: bool,
}

impl NativeGroupSession {
    pub fn connect(mut configs: Vec<NativeGroupMemberConfig>) -> Result<Self, NativeGroupError> {
        if configs.is_empty() {
            return Err(NativeGroupError::EmptyGroup);
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

        let audio_worker =
            WindowsMultiroomAudioWorker::start(targets).map_err(NativeGroupError::Audio)?;

        Ok(Self {
            members,
            audio_worker: Some(audio_worker),
            shared_ptp,
            use_ptp,
        })
    }

    pub fn join_handle(&self) -> Option<NativeGroupJoinHandle> {
        let audio = self.audio_worker.as_ref()?.join_handle();
        Some(NativeGroupJoinHandle {
            shared_ptp: self.shared_ptp.as_ref().map(Arc::clone),
            use_ptp: self.use_ptp,
            audio,
        })
    }

    pub fn adopt_member(&mut self, name: String, session: NativeSession) {
        if let Some(index) = self.members.iter().position(|(member, _)| member == &name) {
            self.members.remove(index);
        }
        self.members.push((name, session));
    }

    pub fn remove_member(&mut self, name: &str) -> Result<(), NativeGroupError> {
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
            total
        })
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
