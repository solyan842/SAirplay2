//! Music Assistant server-style member failure isolation and bounded rejoin policy.
//!
//! Mirrors pinned AirPlay stream/player behavior:
//! - an unexpectedly dead member is isolated from the live session;
//! - surviving members keep playing;
//! - automatic rejoin uses bounded delays 5/15/30/60/120s;
//! - every attempt re-validates that the player is still idle/available and has
//!   not been deliberately regrouped;
//! - only a currently PLAYING candidate with running stream + live session is a
//!   valid target;
//! - success means the failed member actually owns a running stream in that session.

pub const MSA_REJOIN_ATTEMPT_DELAYS_SECS: [u64; 5] = [5, 15, 30, 60, 120];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaPlaybackState {
    Idle,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaRejoinMemberState<'a> {
    pub player_id: &'a str,
    pub available: bool,
    pub playback_state: MsaPlaybackState,
    pub has_group_members: bool,
    pub synced_to: Option<&'a str>,
    pub stream_running: bool,
    pub stream_has_live_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaRejoinDecision {
    CancelActiveAgain,
    CancelUnavailable,
    RetryNoTarget,
    AttemptTarget(usize),
}

pub fn msa_rejoin_attempt_decision(
    failed: &MsaRejoinMemberState<'_>,
    candidate_ids: &[&str],
    candidates: &[MsaRejoinMemberState<'_>],
) -> MsaRejoinDecision {
    if failed.has_group_members
        || failed.stream_running
        || failed.playback_state != MsaPlaybackState::Idle
        || failed
            .synced_to
            .is_some_and(|id| !candidate_ids.iter().any(|candidate| *candidate == id))
    {
        return MsaRejoinDecision::CancelActiveAgain;
    }

    if !failed.available {
        return MsaRejoinDecision::CancelUnavailable;
    }

    for candidate_id in candidate_ids {
        if *candidate_id == failed.player_id {
            continue;
        }
        let Some((idx, candidate)) = candidates
            .iter()
            .enumerate()
            .find(|(_, candidate)| candidate.player_id == *candidate_id)
        else {
            continue;
        };

        // MSA does not follow a candidate absorbed into a different group.
        if candidate.synced_to.is_some() {
            continue;
        }
        if !candidate.available {
            continue;
        }
        if candidate.playback_state != MsaPlaybackState::Playing {
            continue;
        }
        if !candidate.stream_running || !candidate.stream_has_live_session {
            continue;
        }
        return MsaRejoinDecision::AttemptTarget(idx);
    }

    MsaRejoinDecision::RetryNoTarget
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaUnexpectedLossAction {
    IgnoreSuperseded,
    IsolateMember,
    TransferLeaderOrUngroup,
}

/// Decide what an unexpected stream death means before any rejoin scheduling.
///
/// Superseded/replaced streams must never mutate the new owner's group.
/// A non-leader member is isolated from the live session.
/// A dead leader requires leadership transfer/ungroup handling by the controller,
/// while surviving members are preserved.
pub fn msa_unexpected_loss_action(
    ended_cleanly: bool,
    superseded: bool,
    stream_is_current: bool,
    was_leader: bool,
) -> Option<MsaUnexpectedLossAction> {
    if ended_cleanly {
        return None;
    }
    if superseded || !stream_is_current {
        return Some(MsaUnexpectedLossAction::IgnoreSuperseded);
    }
    if was_leader {
        return Some(MsaUnexpectedLossAction::TransferLeaderOrUngroup);
    }
    Some(MsaUnexpectedLossAction::IsolateMember)
}

pub fn msa_rejoin_succeeded(
    stream_running: bool,
    stream_in_live_session: bool,
    member_present_in_session: bool,
) -> bool {
    stream_running && stream_in_live_session && member_present_in_session
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member<'a>(
        id: &'a str,
        available: bool,
        state: MsaPlaybackState,
        synced_to: Option<&'a str>,
        running: bool,
        live: bool,
    ) -> MsaRejoinMemberState<'a> {
        MsaRejoinMemberState {
            player_id: id,
            available,
            playback_state: state,
            has_group_members: false,
            synced_to,
            stream_running: running,
            stream_has_live_session: live,
        }
    }

    #[test]
    fn delay_ladder_matches_msa() {
        assert_eq!(MSA_REJOIN_ATTEMPT_DELAYS_SECS, [5, 15, 30, 60, 120]);
    }

    #[test]
    fn repurposed_failed_player_cancels_rejoin() {
        let failed = member("lost", true, MsaPlaybackState::Playing, None, false, false);
        assert_eq!(
            msa_rejoin_attempt_decision(&failed, &["leader"], &[]),
            MsaRejoinDecision::CancelActiveAgain
        );
    }

    #[test]
    fn unavailable_failed_player_cancels_rejoin() {
        let failed = member("lost", false, MsaPlaybackState::Idle, None, false, false);
        assert_eq!(
            msa_rejoin_attempt_decision(&failed, &["leader"], &[]),
            MsaRejoinDecision::CancelUnavailable
        );
    }

    #[test]
    fn only_live_playing_candidate_is_selected() {
        let failed = member("lost", true, MsaPlaybackState::Idle, None, false, false);
        let candidates = vec![
            member("paused", true, MsaPlaybackState::Paused, None, true, true),
            member("dead", true, MsaPlaybackState::Playing, None, false, true),
            member("leader", true, MsaPlaybackState::Playing, None, true, true),
        ];
        assert_eq!(
            msa_rejoin_attempt_decision(
                &failed,
                &["paused", "dead", "leader"],
                &candidates,
            ),
            MsaRejoinDecision::AttemptTarget(2)
        );
    }

    #[test]
    fn candidate_absorbed_into_other_group_is_not_followed() {
        let failed = member("lost", true, MsaPlaybackState::Idle, None, false, false);
        let candidates = vec![member(
            "old-leader",
            true,
            MsaPlaybackState::Playing,
            Some("different-group"),
            true,
            true,
        )];
        assert_eq!(
            msa_rejoin_attempt_decision(&failed, &["old-leader"], &candidates),
            MsaRejoinDecision::RetryNoTarget
        );
    }

    #[test]
    fn superseded_stream_death_does_not_touch_group() {
        assert_eq!(
            msa_unexpected_loss_action(false, true, true, false),
            Some(MsaUnexpectedLossAction::IgnoreSuperseded)
        );
        assert_eq!(
            msa_unexpected_loss_action(false, false, false, false),
            Some(MsaUnexpectedLossAction::IgnoreSuperseded)
        );
    }

    #[test]
    fn one_dead_member_is_isolated_not_group_stopped() {
        assert_eq!(
            msa_unexpected_loss_action(false, false, true, false),
            Some(MsaUnexpectedLossAction::IsolateMember)
        );
    }

    #[test]
    fn dead_leader_defers_to_leadership_transfer_path() {
        assert_eq!(
            msa_unexpected_loss_action(false, false, true, true),
            Some(MsaUnexpectedLossAction::TransferLeaderOrUngroup)
        );
    }

    #[test]
    fn rejoin_success_requires_live_session_membership() {
        assert!(msa_rejoin_succeeded(true, true, true));
        assert!(!msa_rejoin_succeeded(true, true, false));
        assert!(!msa_rejoin_succeeded(true, false, true));
    }
}
