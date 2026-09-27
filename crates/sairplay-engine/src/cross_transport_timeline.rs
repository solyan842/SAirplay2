//! Source-aligned cross-transport timeline primitives.
//!
//! These are the pure timing contracts shared by Music Assistant's current
//! AirPlay 2 and RAOP orchestration.  Keep them independent from any Windows
//! helper/process so mixed groups can reason about one commanded audible time
//! before either transport-specific implementation is touched.

/// Pinned `raop_session.c` minimum feasibility floor for a commanded audible
/// START.
pub const RAOP_SESSION_MIN_START_LEAD_MS: u64 = 200;

/// Current Music Assistant cold group planning floor.
pub const AIRPLAY_COLD_GROUP_START_LEAD_MS: u64 = 2_500;
pub const AIRPLAY_GROUP_START_LEAD_MS: u64 = 500;
pub const AIRPLAY_SPLICE_LEAD_MARGIN_MS: u64 = 150;
pub const AIRPLAY_START_CONVERGENCE_TOLERANCE_MS: u64 = 2;
pub const AIRPLAY_START_CONVERGENCE_MAX_ROUNDS: usize = 4;

/// Pure decision returned after one Music Assistant group START round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupStartRoundDecision {
    /// Every member landed on the commanded timeline within the 2 ms tolerance.
    Converged { anchor_unix_ms: u64 },
    /// A lone member corrected itself; adopt its true instant and never re-START it.
    SoloCorrected { anchor_unix_ms: u64 },
    /// A multi-member group needs another concurrent START round.
    Retry { next_target_unix_ms: u64, corrected_unix_ms: u64 },
}

/// Apply current Music Assistant `_start_members()` convergence arithmetic to
/// one completed concurrent START round.
///
/// Each tuple is `(sync_adjust_ms, acknowledged_member_unix_ms)`. The
/// acknowledgement is first brought back to the common, unadjusted group
/// timeline, then the largest true instant wins. This function deliberately
/// does no I/O and no transport routing; both native AP2 and RAOP can feed the
/// same verified acknowledgement shape into it.
pub fn evaluate_group_start_round(
    target_unix_ms: u64,
    member_acks: &[(i64, u64)],
) -> GroupStartRoundDecision {
    let mut corrected = target_unix_ms;

    for (adjust_ms, acknowledged_unix_ms) in member_acks {
        let unadjusted = if *adjust_ms >= 0 {
            acknowledged_unix_ms.saturating_sub(*adjust_ms as u64)
        } else {
            acknowledged_unix_ms.saturating_add(adjust_ms.unsigned_abs())
        };
        corrected = corrected.max(unadjusted);
    }

    if corrected <= target_unix_ms.saturating_add(AIRPLAY_START_CONVERGENCE_TOLERANCE_MS) {
        return GroupStartRoundDecision::Converged {
            anchor_unix_ms: target_unix_ms,
        };
    }

    if member_acks.len() == 1 {
        return GroupStartRoundDecision::SoloCorrected {
            anchor_unix_ms: corrected,
        };
    }

    GroupStartRoundDecision::Retry {
        next_target_unix_ms: corrected.saturating_add(AIRPLAY_SPLICE_LEAD_MARGIN_MS),
        corrected_unix_ms: corrected,
    }
}

/// Result of resolving a commanded RAOP audible START against the moving
/// feasibility floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaopResolvedStart {
    /// Requested audible unix time, 0 when the caller asked for earliest start.
    pub requested_unix_ms: u64,
    /// Audible unix time actually committed by the RAOP session contract.
    pub committed_unix_ms: u64,
    /// Positive forward correction applied to an infeasible non-zero request.
    pub correction_ms: u64,
}

/// Exact arithmetic of pinned `raop_session.c::resolve_start`.
///
/// - A feasible non-zero request is honored exactly.
/// - A non-zero request behind `now + 200 ms` is corrected to
///   `now + 400 ms` so a corrective retry cannot chase a moving floor.
/// - A zero request takes the earliest floor directly: `now + 200 ms`.
pub fn resolve_raop_start_unix_ms(now_unix_ms: u64, requested_unix_ms: u64) -> RaopResolvedStart {
    let floor = now_unix_ms.saturating_add(RAOP_SESSION_MIN_START_LEAD_MS);
    let committed_unix_ms = if requested_unix_ms != 0 && requested_unix_ms >= floor {
        requested_unix_ms
    } else if requested_unix_ms != 0 {
        floor.saturating_add(RAOP_SESSION_MIN_START_LEAD_MS)
    } else {
        floor
    };
    RaopResolvedStart {
        requested_unix_ms,
        committed_unix_ms,
        correction_ms: if requested_unix_ms == 0 {
            0
        } else {
            committed_unix_ms.saturating_sub(requested_unix_ms)
        },
    }
}

/// Exact RAOP audible-head projection from pinned
/// `raop_session_next_head_unix_ms`.
///
/// `playtime_ntp` is the audible NTP instant returned by libraop for the
/// chunk that was just sent. The next delivery head is that playtime plus the
/// chunk duration.
pub fn raop_next_head_unix_ms(
    playtime_ntp: u64,
    chunk_frames: u32,
    sample_rate: u32,
) -> u64 {
    if playtime_ntp == 0 || sample_rate == 0 {
        return 0;
    }
    let frame_ntp = (((chunk_frames as u128) << 32) / sample_rate as u128) as u64;
    ntp_to_unix_ms(playtime_ntp.saturating_add(frame_ntp))
}

fn ntp_to_unix_ms(ntp: u64) -> u64 {
    (ntp >> 32)
        .saturating_mul(1000)
        .saturating_add((((ntp & 0xFFFF_FFFF) as u128 * 1000) >> 32) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix_ms_to_ntp(ms: u64) -> u64 {
        ((ms / 1000) << 32) | ((((ms % 1000) as u128) << 32) / 1000) as u64
    }

    #[test]
    fn mixed_transport_round_converges_within_two_ms() {
        assert_eq!(
            evaluate_group_start_round(10_000, &[(0, 10_000), (0, 10_002)]),
            GroupStartRoundDecision::Converged {
                anchor_unix_ms: 10_000
            }
        );
    }

    #[test]
    fn mixed_transport_round_retries_at_largest_true_ack_plus_margin() {
        assert_eq!(
            evaluate_group_start_round(10_000, &[(0, 10_000), (0, 10_240)]),
            GroupStartRoundDecision::Retry {
                next_target_unix_ms: 10_390,
                corrected_unix_ms: 10_240,
            }
        );
    }

    #[test]
    fn convergence_removes_per_member_sync_adjust_before_comparing() {
        assert_eq!(
            evaluate_group_start_round(
                10_000,
                &[
                    (25, 10_025),
                    (-40, 9_960),
                ],
            ),
            GroupStartRoundDecision::Converged {
                anchor_unix_ms: 10_000
            }
        );
    }

    #[test]
    fn solo_correction_is_adopted_without_second_start() {
        assert_eq!(
            evaluate_group_start_round(10_000, &[(0, 10_300)]),
            GroupStartRoundDecision::SoloCorrected {
                anchor_unix_ms: 10_300
            }
        );
    }

    #[test]
    fn convergence_constants_match_current_music_assistant() {
        assert_eq!(AIRPLAY_GROUP_START_LEAD_MS, 500);
        assert_eq!(AIRPLAY_SPLICE_LEAD_MARGIN_MS, 150);
        assert_eq!(AIRPLAY_START_CONVERGENCE_TOLERANCE_MS, 2);
        assert_eq!(AIRPLAY_START_CONVERGENCE_MAX_ROUNDS, 4);
    }

    #[test]
    fn feasible_raop_start_is_honored_exactly() {
        let resolved = resolve_raop_start_unix_ms(10_000, 12_500);
        assert_eq!(
            resolved,
            RaopResolvedStart {
                requested_unix_ms: 12_500,
                committed_unix_ms: 12_500,
                correction_ms: 0,
            }
        );
    }

    #[test]
    fn zero_raop_start_uses_the_200ms_floor() {
        let resolved = resolve_raop_start_unix_ms(10_000, 0);
        assert_eq!(resolved.committed_unix_ms, 10_200);
        assert_eq!(resolved.correction_ms, 0);
    }

    #[test]
    fn infeasible_raop_start_gets_one_extra_floor_of_retry_slack() {
        let resolved = resolve_raop_start_unix_ms(10_000, 10_100);
        assert_eq!(resolved.committed_unix_ms, 10_400);
        assert_eq!(resolved.correction_ms, 300);
    }

    #[test]
    fn exact_floor_is_feasible() {
        let resolved = resolve_raop_start_unix_ms(10_000, 10_200);
        assert_eq!(resolved.committed_unix_ms, 10_200);
        assert_eq!(resolved.correction_ms, 0);
    }

    #[test]
    fn raop_head_projection_matches_pinned_source_integer_truncation() {
        // Pinned airplay-cli tests this exact contract: a 352-frame chunk
        // projects seven whole milliseconds at both 44.1 and 48 kHz because
        // the source converts through fixed-point NTP and truncates to unix ms.
        let playtime = unix_ms_to_ntp(50_000);
        assert_eq!(raop_next_head_unix_ms(playtime, 352, 44_100), 50_007);
        assert_eq!(raop_next_head_unix_ms(playtime, 352, 48_000), 50_007);
    }

    #[test]
    fn unknown_raop_playtime_has_no_head() {
        assert_eq!(raop_next_head_unix_ms(0, 352, 44_100), 0);
    }

    #[test]
    fn cold_group_planning_floor_matches_current_music_assistant() {
        assert_eq!(AIRPLAY_COLD_GROUP_START_LEAD_MS, 2_500);
    }
}
