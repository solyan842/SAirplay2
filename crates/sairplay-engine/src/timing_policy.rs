use crate::ReceiverCapabilities;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingPreference {
    Auto,
    ForcePtp,
    ForceNtp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingMode {
    Ptp,
    Ntp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingStartResult {
    Ready,
    Unavailable,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingDecision {
    pub requested: TimingMode,
    pub effective: TimingMode,
    pub fell_back: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingReadiness {
    Pending,
    Ready(TimingDecision),
    Failed,
}

impl TimingDecision {
    pub fn select(caps: ReceiverCapabilities, preference: TimingPreference) -> TimingMode {
        match preference {
            TimingPreference::ForcePtp => TimingMode::Ptp,
            TimingPreference::ForceNtp => TimingMode::Ntp,
            TimingPreference::Auto => {
                if caps.supports_ptp {
                    TimingMode::Ptp
                } else {
                    TimingMode::Ntp
                }
            }
        }
    }

    pub fn resolve_startup(
        requested: TimingMode,
        ptp_start: TimingStartResult,
        ntp_start: TimingStartResult,
    ) -> TimingReadiness {
        match requested {
            TimingMode::Ptp => match ptp_start {
                TimingStartResult::Ready => TimingReadiness::Ready(TimingDecision {
                    requested,
                    effective: TimingMode::Ptp,
                    fell_back: false,
                }),
                TimingStartResult::Unavailable | TimingStartResult::Failed => match ntp_start {
                    TimingStartResult::Ready => TimingReadiness::Ready(TimingDecision {
                        requested,
                        effective: TimingMode::Ntp,
                        fell_back: true,
                    }),
                    _ => TimingReadiness::Failed,
                },
            },
            TimingMode::Ntp => match ntp_start {
                TimingStartResult::Ready => TimingReadiness::Ready(TimingDecision {
                    requested,
                    effective: TimingMode::Ntp,
                    fell_back: false,
                }),
                _ => TimingReadiness::Failed,
            },
        }
    }

    pub fn timing_protocol(self) -> &'static str {
        match self.effective {
            TimingMode::Ptp => "PTP",
            TimingMode::Ntp => "NTP",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_ptp_only_when_receiver_advertises_support() {
        let ptp = ReceiverCapabilities {
            supports_ptp: true,
            ..Default::default()
        };
        let no_ptp = ReceiverCapabilities::default();

        assert_eq!(
            TimingDecision::select(ptp, TimingPreference::Auto),
            TimingMode::Ptp
        );
        assert_eq!(
            TimingDecision::select(no_ptp, TimingPreference::Auto),
            TimingMode::Ntp
        );
    }

    #[test]
    fn explicit_preference_overrides_txt_feature_bit() {
        let caps = ReceiverCapabilities {
            supports_ptp: true,
            ..Default::default()
        };

        assert_eq!(
            TimingDecision::select(caps, TimingPreference::ForceNtp),
            TimingMode::Ntp
        );
        assert_eq!(
            TimingDecision::select(ReceiverCapabilities::default(), TimingPreference::ForcePtp),
            TimingMode::Ptp
        );
    }

    #[test]
    fn ptp_start_failure_falls_back_to_ready_ntp() {
        let ready = TimingDecision::resolve_startup(
            TimingMode::Ptp,
            TimingStartResult::Unavailable,
            TimingStartResult::Ready,
        );

        assert_eq!(
            ready,
            TimingReadiness::Ready(TimingDecision {
                requested: TimingMode::Ptp,
                effective: TimingMode::Ntp,
                fell_back: true,
            })
        );
    }

    #[test]
    fn forced_ntp_does_not_try_ptp_and_requires_ntp_readiness() {
        assert_eq!(
            TimingDecision::resolve_startup(
                TimingMode::Ntp,
                TimingStartResult::Ready,
                TimingStartResult::Failed,
            ),
            TimingReadiness::Failed
        );
    }

    #[test]
    fn timing_is_not_ready_when_both_engines_fail() {
        assert_eq!(
            TimingDecision::resolve_startup(
                TimingMode::Ptp,
                TimingStartResult::Failed,
                TimingStartResult::Failed,
            ),
            TimingReadiness::Failed
        );
    }

    #[test]
    fn setup_protocol_uses_effective_not_requested_mode() {
        let readiness = TimingDecision::resolve_startup(
            TimingMode::Ptp,
            TimingStartResult::Failed,
            TimingStartResult::Ready,
        );

        let TimingReadiness::Ready(decision) = readiness else {
            panic!("expected fallback readiness");
        };

        assert_eq!(decision.requested, TimingMode::Ptp);
        assert_eq!(decision.effective, TimingMode::Ntp);
        assert_eq!(decision.timing_protocol(), "NTP");
    }
}
