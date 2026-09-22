use crate::{
    NativeSession, NativeSessionConfig, NativeVolumeControl, PtpEngine, RetransmitStats,
    WindowsMultiroomAudioError, WindowsMultiroomAudioWorker,
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
    TooFewMembers,
    Member { name: String, error: String },
    Audio(WindowsMultiroomAudioError),
}

impl fmt::Display for NativeGroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooFewMembers => write!(f, "MultiRoom requires at least two native receivers"),
            Self::Member { name, error } => write!(f, "{name}: {error}"),
            Self::Audio(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NativeGroupError {}

pub struct NativeGroupSession {
    members: Vec<(String, NativeSession)>,
    audio_worker: Option<WindowsMultiroomAudioWorker>,
}

impl NativeGroupSession {
    pub fn connect(mut configs: Vec<NativeGroupMemberConfig>) -> Result<Self, NativeGroupError> {
        if configs.len() < 2 {
            return Err(NativeGroupError::TooFewMembers);
        }

        // Source architecture: one shared PTP daemon owns 319/320 for all AP2
        // members. Connect a PTP-capable member first so its engine becomes the
        // shared clock. Grouped receivers never use the standalone HomePod
        // follow-clock exception.
        if let Some(index) = configs.iter().position(|member| member.config.supports_ptp) {
            configs.swap(0, index);
        }

        let group_wants_ptp = configs.iter().any(|member| member.config.supports_ptp);
        let mut shared_ptp: Option<Arc<PtpEngine>> = None;
        let mut members = Vec::<(String, NativeSession)>::with_capacity(configs.len());

        for (index, mut member) in configs.into_iter().enumerate() {
            if group_wants_ptp && member.config.supports_ptp {
                member.config.follow_receiver_clock = false;
            }

            let session_result = if index == 0 {
                NativeSession::connect(&member.config)
            } else if member.config.supports_ptp {
                if let Some(engine) = shared_ptp.as_ref() {
                    NativeSession::connect_with_shared_ptp(
                        &member.config,
                        Some(Arc::clone(engine)),
                    )
                } else {
                    // The first PTP candidate fell back because a PTP engine
                    // could not be established. Keep the group timing decision
                    // coherent and force later AP2 members onto NTP as well.
                    member.config.supports_ptp = false;
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
        })
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
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
        // Source disconnect order at the group level: stop the common producer
        // first, then let each NativeSession close event/feedback/RTSP timing.
        self.stop_audio();
    }
}
