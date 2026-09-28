//! AP2 cold-clock post-commit verification for the SAirplay 2.0 MSA clone.
//!
//! Mirrors pinned `ap2_clock_verify_arm()` + `ap2cl_clock_verify_poll()`.
//! A cold-clock origin START is observed only. A cold-clock JOIN may be moved
//! forward to receiver readiness, but only while no real audio has gone out.
//! A deferred START ack and a content cut are mutually exclusive remedies.

pub const MSA_AP2_CLOCK_VERIFY_POLL_MS: u64 = 250;
pub const MSA_AP2_CLOCK_VERIFY_EXTRA_WINDOW_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaAp2VerifyArm {
    pub requested_unix_ms: u64,
    pub anchor_unix_ms: u64,
    pub enforce_join: bool,
    pub defer_start_ack: bool,
}

pub fn msa_ap2_verify_arm(
    now_unix_ms: u64,
    requested_unix_ms: u64,
    anchor_unix_ms: u64,
    splice_depth_ms: u64,
    is_join: bool,
) -> Option<MsaAp2VerifyArm> {
    let min_window = splice_depth_ms.saturating_add(MSA_AP2_CLOCK_VERIFY_EXTRA_WINDOW_MS);
    if anchor_unix_ms < now_unix_ms.saturating_add(min_window) {
        return None;
    }

    Some(MsaAp2VerifyArm {
        requested_unix_ms,
        anchor_unix_ms,
        enforce_join: is_join,
        defer_start_ack: is_join,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaAp2VerifyResult {
    Idle,
    Verified,
    Corrected,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaAp2VerifyEvent {
    pub result: MsaAp2VerifyResult,
    pub requested_unix_ms: u64,
    pub from_unix_ms: u64,
    pub at_unix_ms: u64,
    pub margin_ms: i64,
    pub content_cut_ms: u64,
    pub start_ack: bool,
}

impl MsaAp2VerifyEvent {
    fn idle() -> Self {
        Self {
            result: MsaAp2VerifyResult::Idle,
            requested_unix_ms: 0,
            from_unix_ms: 0,
            at_unix_ms: 0,
            margin_ms: 0,
            content_cut_ms: 0,
            start_ack: false,
        }
    }
}

pub fn msa_ap2_verify_poll(
    arm: MsaAp2VerifyArm,
    now_unix_ms: u64,
    receiver_ready_at_unix_ms: Option<u64>,
    audio_sent_since_arm: bool,
    splice_depth_ms: u64,
) -> MsaAp2VerifyEvent {
    let anchor = arm.anchor_unix_ms;

    if let Some(ready) = receiver_ready_at_unix_ms {
        if ready <= anchor {
            return MsaAp2VerifyEvent {
                result: MsaAp2VerifyResult::Verified,
                requested_unix_ms: arm.requested_unix_ms,
                from_unix_ms: anchor,
                at_unix_ms: anchor,
                margin_ms: (anchor - ready) as i64,
                content_cut_ms: 0,
                start_ack: arm.defer_start_ack,
            };
        }

        if !arm.enforce_join {
            // Origin START: observe only. Moving one member of a group origin
            // after commit would desynchronise the group.
            return MsaAp2VerifyEvent {
                result: MsaAp2VerifyResult::Unverified,
                requested_unix_ms: arm.requested_unix_ms,
                from_unix_ms: anchor,
                at_unix_ms: anchor,
                margin_ms: -((ready - anchor) as i64),
                content_cut_ms: 0,
                start_ack: arm.defer_start_ack,
            };
        }

        if !audio_sent_since_arm {
            // Join correction goes exactly to readiness: no extra 250 ms is
            // needed because this correction itself has no command retry.
            let cut = if arm.defer_start_ack {
                0
            } else {
                ready - anchor
            };
            return MsaAp2VerifyEvent {
                result: MsaAp2VerifyResult::Corrected,
                requested_unix_ms: arm.requested_unix_ms,
                from_unix_ms: anchor,
                at_unix_ms: ready,
                margin_ms: -((ready - anchor) as i64),
                content_cut_ms: cut,
                start_ack: arm.defer_start_ack,
            };
        }

        // Once real frames are on the wire the frozen line cannot move without
        // a timestamp jump. Session-level resync is the remaining remedy.
        return MsaAp2VerifyEvent {
            result: MsaAp2VerifyResult::Unverified,
            requested_unix_ms: arm.requested_unix_ms,
            from_unix_ms: anchor,
            at_unix_ms: anchor,
            margin_ms: -((ready - anchor) as i64),
            content_cut_ms: 0,
            start_ack: arm.defer_start_ack,
        };
    }

    // No receiver probe yet. Once audio has escaped, or one poll round before
    // the pacing window closes, verification can no longer act.
    let close_at = anchor.saturating_sub(splice_depth_ms);
    if audio_sent_since_arm
        || now_unix_ms.saturating_add(MSA_AP2_CLOCK_VERIFY_POLL_MS) >= close_at
    {
        return MsaAp2VerifyEvent {
            result: MsaAp2VerifyResult::Unverified,
            requested_unix_ms: arm.requested_unix_ms,
            from_unix_ms: anchor,
            at_unix_ms: anchor,
            margin_ms: 0,
            content_cut_ms: 0,
            start_ack: arm.defer_start_ack,
        };
    }

    MsaAp2VerifyEvent::idle()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_arms_only_with_splice_depth_plus_500ms_runway() {
        assert!(msa_ap2_verify_arm(1_000, 0, 2_100, 600, true).is_some());
        assert!(msa_ap2_verify_arm(1_000, 0, 2_099, 600, true).is_none());
    }

    #[test]
    fn origin_start_is_observe_only_even_when_clock_is_late() {
        let arm = msa_ap2_verify_arm(1_000, 2_000, 3_000, 600, false).unwrap();
        let ev = msa_ap2_verify_poll(arm, 1_500, Some(3_400), false, 600);

        assert_eq!(ev.result, MsaAp2VerifyResult::Unverified);
        assert_eq!(ev.at_unix_ms, 3_000);
        assert_eq!(ev.content_cut_ms, 0);
        assert!(!ev.start_ack);
    }

    #[test]
    fn cold_join_defers_ack_and_corrects_to_readiness_before_audio() {
        let arm = msa_ap2_verify_arm(1_000, 2_000, 3_000, 600, true).unwrap();
        let ev = msa_ap2_verify_poll(arm, 1_500, Some(3_400), false, 600);

        assert_eq!(ev.result, MsaAp2VerifyResult::Corrected);
        assert_eq!(ev.at_unix_ms, 3_400);
        assert_eq!(ev.content_cut_ms, 0);
        assert!(ev.start_ack);
    }

    #[test]
    fn already_acked_join_uses_content_cut_instead_of_double_correction() {
        let mut arm = msa_ap2_verify_arm(1_000, 2_000, 3_000, 600, true).unwrap();
        arm.defer_start_ack = false;
        let ev = msa_ap2_verify_poll(arm, 1_500, Some(3_400), false, 600);

        assert_eq!(ev.result, MsaAp2VerifyResult::Corrected);
        assert_eq!(ev.content_cut_ms, 400);
        assert!(!ev.start_ack);
    }

    #[test]
    fn late_readiness_after_audio_cannot_move_anchor() {
        let arm = msa_ap2_verify_arm(1_000, 2_000, 3_000, 600, true).unwrap();
        let ev = msa_ap2_verify_poll(arm, 2_500, Some(3_400), true, 600);

        assert_eq!(ev.result, MsaAp2VerifyResult::Unverified);
        assert_eq!(ev.at_unix_ms, 3_000);
        assert!(ev.start_ack);
    }

    #[test]
    fn receiver_ready_before_anchor_verifies_with_positive_margin() {
        let arm = msa_ap2_verify_arm(1_000, 2_000, 3_000, 600, true).unwrap();
        let ev = msa_ap2_verify_poll(arm, 1_500, Some(2_700), false, 600);

        assert_eq!(ev.result, MsaAp2VerifyResult::Verified);
        assert_eq!(ev.margin_ms, 300);
        assert!(ev.start_ack);
    }

    #[test]
    fn no_probe_stays_idle_until_action_window_closes() {
        let arm = msa_ap2_verify_arm(1_000, 2_000, 3_000, 600, true).unwrap();

        let early = msa_ap2_verify_poll(arm, 2_000, None, false, 600);
        assert_eq!(early.result, MsaAp2VerifyResult::Idle);

        // close_at=2400; at 2150 plus one 250ms poll, no correction runway remains.
        let closed = msa_ap2_verify_poll(arm, 2_150, None, false, 600);
        assert_eq!(closed.result, MsaAp2VerifyResult::Unverified);
        assert!(closed.start_ack);
    }
}
