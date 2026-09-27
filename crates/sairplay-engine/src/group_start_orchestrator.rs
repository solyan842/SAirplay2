//! Transport-neutral concurrent group START round orchestration.
//!
//! Music Assistant's `_start_members()` launches every member `stream.start()`
//! in one TaskGroup, then evaluates the TRUE scheduled instant returned by each
//! transport. This module mirrors only that I/O shape. Timeline convergence
//! arithmetic remains in `cross_transport_timeline`, and PCM/session ownership
//! remains transport-specific until a real shared mixed-transport worker exists.

use crate::cross_transport_timeline::{
    evaluate_group_start_round, GroupStartRoundDecision,
    AIRPLAY_START_CONVERGENCE_MAX_ROUNDS,
};
use std::fmt;
use std::thread;

/// One transport member that can commit a commanded audible START and report
/// the instant it actually scheduled, in Unix epoch milliseconds.
pub trait GroupStartParticipant: Send {
    fn name(&self) -> &str;

    /// Per-member Music Assistant sync adjustment. Windows has no exposed
    /// adjustment control yet, so transport implementations inherit zero.
    fn sync_adjust_ms(&self) -> i64 {
        0
    }

    /// Commit one START and return the TRUE scheduled audible Unix-ms instant.
    fn start_at_unix_ms(&mut self, requested_start_unix_ms: u64) -> Result<u64, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStartIoError {
    pub member: String,
    pub error: String,
}

impl fmt::Display for GroupStartIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.member, self.error)
    }
}

impl std::error::Error for GroupStartIoError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStartCorrection {
    pub round: usize,
    pub requested_unix_ms: u64,
    pub corrected_unix_ms: u64,
    pub retry_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStartConvergence {
    pub anchor_unix_ms: u64,
    pub rounds: usize,
    pub converged: bool,
    pub corrections: Vec<GroupStartCorrection>,
}

/// Execute Music Assistant's complete group START convergence loop over a
/// transport-neutral round runner. The runner performs exactly one concurrent
/// START round and returns true scheduled member instants.
pub fn run_group_start_convergence<F>(
    initial_target_unix_ms: u64,
    mut run_round: F,
) -> Result<GroupStartConvergence, GroupStartIoError>
where
    F: FnMut(u64) -> Result<Vec<(i64, u64)>, GroupStartIoError>,
{
    let mut target_unix_ms = initial_target_unix_ms;
    let mut anchor_unix_ms = initial_target_unix_ms;
    let mut corrections = Vec::new();

    for round in 1..=AIRPLAY_START_CONVERGENCE_MAX_ROUNDS {
        let member_acks = run_round(target_unix_ms)?;
        match evaluate_group_start_round(target_unix_ms, &member_acks) {
            GroupStartRoundDecision::Converged { anchor_unix_ms: anchor }
            | GroupStartRoundDecision::SoloCorrected { anchor_unix_ms: anchor } => {
                return Ok(GroupStartConvergence {
                    anchor_unix_ms: anchor,
                    rounds: round,
                    converged: true,
                    corrections,
                });
            }
            GroupStartRoundDecision::Retry {
                next_target_unix_ms,
                corrected_unix_ms,
            } => {
                anchor_unix_ms = corrected_unix_ms;
                corrections.push(GroupStartCorrection {
                    round,
                    requested_unix_ms: target_unix_ms,
                    corrected_unix_ms,
                    retry_unix_ms: next_target_unix_ms,
                });
                if round < AIRPLAY_START_CONVERGENCE_MAX_ROUNDS {
                    target_unix_ms = next_target_unix_ms;
                }
            }
        }
    }

    Ok(GroupStartConvergence {
        anchor_unix_ms,
        rounds: AIRPLAY_START_CONVERGENCE_MAX_ROUNDS,
        converged: false,
        corrections,
    })
}

/// Fan one commanded audible instant out to every participant concurrently and
/// collect the MSA convergence acknowledgement shape:
/// `(sync_adjust_ms, acknowledged_member_unix_ms)`.
///
/// Taking trait objects deliberately allows a future round to contain native
/// AirPlay 2 and RAOP participants together. This function does NOT create a
/// mixed session or join separate audio workers; it only normalizes START I/O.
pub fn run_concurrent_group_start_round(
    members: Vec<&mut dyn GroupStartParticipant>,
    requested_start_unix_ms: u64,
) -> Result<Vec<(i64, u64)>, GroupStartIoError> {
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(members.len());

        for member in members {
            handles.push(scope.spawn(move || {
                let name = member.name().to_owned();
                let adjust_ms = member.sync_adjust_ms();
                member
                    .start_at_unix_ms(requested_start_unix_ms)
                    .map(|acknowledged_unix_ms| (adjust_ms, acknowledged_unix_ms))
                    .map_err(|error| GroupStartIoError {
                        member: name,
                        error,
                    })
            }));
        }

        let mut acknowledgements = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.join() {
                Ok(Ok(ack)) => acknowledgements.push(ack),
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    return Err(GroupStartIoError {
                        member: "AirPlay group".to_owned(),
                        error: "concurrent START worker panicked".to_owned(),
                    })
                }
            }
        }
        Ok(acknowledgements)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeMember {
        name: &'static str,
        adjust_ms: i64,
        acknowledged_unix_ms: u64,
        fail: bool,
    }

    impl GroupStartParticipant for FakeMember {
        fn name(&self) -> &str {
            self.name
        }

        fn sync_adjust_ms(&self) -> i64 {
            self.adjust_ms
        }

        fn start_at_unix_ms(
            &mut self,
            _requested_start_unix_ms: u64,
        ) -> Result<u64, String> {
            if self.fail {
                Err("start failed".to_owned())
            } else {
                Ok(self.acknowledged_unix_ms)
            }
        }
    }

    #[test]
    fn heterogeneous_ready_shape_preserves_adjustment_and_true_ack() {
        let mut first = FakeMember {
            name: "native",
            adjust_ms: 25,
            acknowledged_unix_ms: 10_025,
            fail: false,
        };
        let mut second = FakeMember {
            name: "raop",
            adjust_ms: -40,
            acknowledged_unix_ms: 9_960,
            fail: false,
        };

        let acks = run_concurrent_group_start_round(
            vec![&mut first, &mut second],
            10_000,
        )
        .unwrap();

        assert_eq!(acks, vec![(25, 10_025), (-40, 9_960)]);
    }

    #[test]
    fn convergence_retries_then_anchors_on_common_round() {
        let mut rounds = Vec::new();
        let result = run_group_start_convergence(10_000, |target| {
            rounds.push(target);
            if rounds.len() == 1 {
                Ok(vec![(0, 10_000), (0, 10_240)])
            } else {
                Ok(vec![(0, target), (0, target + 1)])
            }
        })
        .unwrap();

        assert_eq!(rounds, vec![10_000, 10_390]);
        assert_eq!(result.anchor_unix_ms, 10_390);
        assert_eq!(result.rounds, 2);
        assert!(result.converged);
        assert_eq!(result.corrections.len(), 1);
    }

    #[test]
    fn convergence_exhaustion_keeps_last_ack_not_unsent_retry() {
        let mut last_commanded = 0;
        let result = run_group_start_convergence(10_000, |target| {
            last_commanded = target;
            Ok(vec![(0, target + 100), (0, target)])
        })
        .unwrap();

        assert!(!result.converged);
        assert_eq!(result.rounds, AIRPLAY_START_CONVERGENCE_MAX_ROUNDS);
        assert_eq!(result.anchor_unix_ms, last_commanded + 100);
    }

    #[test]
    fn member_failure_keeps_member_identity() {
        let mut failing = FakeMember {
            name: "receiver-b",
            adjust_ms: 0,
            acknowledged_unix_ms: 0,
            fail: true,
        };

        let error =
            run_concurrent_group_start_round(vec![&mut failing], 10_000).unwrap_err();

        assert_eq!(error.member, "receiver-b");
        assert_eq!(error.error, "start failed");
    }
}
