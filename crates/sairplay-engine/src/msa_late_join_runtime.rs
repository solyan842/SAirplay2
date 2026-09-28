//! Music Assistant server-style late-join runtime contract.
//!
//! Mirrors pinned `AirPlayStreamSession.add_client()` ordering:
//! 1. Plan a join anchor from live timeline + receiver readiness.
//! 2. START the joiner BEFORE writing prime audio.
//! 3. Await the joiner's true START ACK outside the group feed lock.
//! 4. Re-snapshot the live timeline/ring.
//! 5. Re-map content onto the acknowledged instant.
//! 6. Prime or schedule live-feed skip for the joiner only.
//!
//! Existing group members are never re-anchored for a late join.

use crate::{
    msa_late_join_requested_anchor_unix_ms, msa_plan_late_join, MsaLateJoinPlan,
    MsaLateJoinSnapshot, MSA_CLOCK_READY_LEAD_MS,
};

pub trait MsaLateJoinRuntime {
    /// Joiner's configured per-member sync adjustment.
    fn sync_adjust_ms(&self) -> i64;

    /// Send START(join=true) and return the true audible instant ACK.
    fn start_join(
        &mut self,
        commanded_unix_ms: u64,
        position_ms: u64,
    ) -> Result<u64, String>;

    /// Rebase the joiner's reported position after ACK correction.
    fn rebase_position(&mut self, position_ms: u64);

    /// Write the post-ACK prime into the already-started joiner.
    fn write_prime(&mut self, prime: &[u8]) -> Result<(), String>;

    /// Record how many bytes to skip from this joiner's future live feed.
    fn set_live_skip_bytes(&mut self, bytes: u64);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaLateJoinStartRequest {
    pub requested_group_anchor_unix_ms: u64,
    pub commanded_member_anchor_unix_ms: u64,
    pub position_ms: u64,
    pub precommit_plan: MsaLateJoinPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaLateJoinCommitResult {
    pub actual_member_anchor_unix_ms: u64,
    pub actual_group_anchor_unix_ms: u64,
    pub position_ms: u64,
    pub committed_plan: MsaLateJoinPlan,
}

fn apply_adjust(anchor: u64, adjust: i64) -> u64 {
    if adjust >= 0 {
        anchor.saturating_add(adjust as u64)
    } else {
        anchor.saturating_sub(adjust.unsigned_abs())
    }
}

fn remove_adjust(anchor: u64, adjust: i64) -> u64 {
    if adjust >= 0 {
        anchor.saturating_sub(adjust as u64)
    } else {
        anchor.saturating_add(adjust.unsigned_abs())
    }
}

/// Build the pre-START late-join request under the live-session lock.
pub fn msa_prepare_late_join(
    snapshot: &MsaLateJoinSnapshot<'_>,
    media_base_position_ms: u64,
    sync_adjust_ms: i64,
) -> Result<MsaLateJoinStartRequest, String> {
    let requested = msa_late_join_requested_anchor_unix_ms(
        snapshot.now_unix_ms,
        snapshot.ready_at_unix_ms,
        MSA_CLOCK_READY_LEAD_MS,
    );
    let plan = msa_plan_late_join(snapshot, requested, false)?;

    // MSA sends position mapped to the sample due at the planned anchor.
    let position_ms = media_base_position_ms.saturating_add(
        ((plan.due_position_bytes as u128 * 1000) / snapshot.pcm_byte_rate as u128)
            as u64,
    );

    Ok(MsaLateJoinStartRequest {
        requested_group_anchor_unix_ms: plan.requested_start_unix_ms,
        commanded_member_anchor_unix_ms: apply_adjust(
            plan.requested_start_unix_ms,
            sync_adjust_ms,
        ),
        position_ms,
        precommit_plan: plan,
    })
}

/// Execute START only. Call this outside the live group feed lock.
pub fn msa_start_late_join(
    runtime: &mut dyn MsaLateJoinRuntime,
    request: &MsaLateJoinStartRequest,
) -> Result<u64, String> {
    runtime.start_join(
        request.commanded_member_anchor_unix_ms,
        request.position_ms,
    )
}

/// Commit the join under the live-session lock using a NEW snapshot taken
/// after the START ACK. This is the key MSA invariant: content is mapped to
/// verified receiver truth, not the original request.
pub fn msa_commit_late_join(
    runtime: &mut dyn MsaLateJoinRuntime,
    post_ack_snapshot: &MsaLateJoinSnapshot<'_>,
    media_base_position_ms: u64,
    actual_member_anchor_unix_ms: u64,
) -> Result<MsaLateJoinCommitResult, String> {
    let adjust = runtime.sync_adjust_ms();
    let actual_group_anchor = remove_adjust(actual_member_anchor_unix_ms, adjust);

    let plan = msa_plan_late_join(post_ack_snapshot, actual_group_anchor, true)?;

    let position_ms = media_base_position_ms.saturating_add(
        ((plan.due_position_bytes as u128 * 1000)
            / post_ack_snapshot.pcm_byte_rate as u128) as u64,
    );

    runtime.set_live_skip_bytes(plan.skip_live_bytes);
    runtime.rebase_position(position_ms);

    // START already happened. Prime is deliberately written only now so a
    // prime larger than the bounded pre-START session ring cannot wedge the
    // join and block the existing group's live feed.
    if !plan.prime.is_empty() {
        runtime.write_prime(&plan.prime)?;
    }

    Ok(MsaLateJoinCommitResult {
        actual_member_anchor_unix_ms,
        actual_group_anchor_unix_ms: actual_group_anchor,
        position_ms,
        committed_plan: plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRuntime {
        adjust: i64,
        start_ack: u64,
        calls: Vec<&'static str>,
        rebased: Option<u64>,
        prime_len: usize,
        skip: u64,
    }

    impl MsaLateJoinRuntime for FakeRuntime {
        fn sync_adjust_ms(&self) -> i64 { self.adjust }

        fn start_join(
            &mut self,
            _commanded_unix_ms: u64,
            _position_ms: u64,
        ) -> Result<u64, String> {
            self.calls.push("start");
            Ok(self.start_ack)
        }

        fn rebase_position(&mut self, position_ms: u64) {
            self.calls.push("rebase");
            self.rebased = Some(position_ms);
        }

        fn write_prime(&mut self, prime: &[u8]) -> Result<(), String> {
            self.calls.push("prime");
            self.prime_len = prime.len();
            Ok(())
        }

        fn set_live_skip_bytes(&mut self, bytes: u64) {
            self.calls.push("skip");
            self.skip = bytes;
        }
    }

    fn snapshot<'a>(
        ring: &'a [u8],
        now: u64,
        effective_start: u64,
        seconds_streamed: f64,
        total_fed: u64,
    ) -> MsaLateJoinSnapshot<'a> {
        MsaLateJoinSnapshot {
            now_unix_ms: now,
            effective_start_unix_ms: effective_start,
            ready_at_unix_ms: 0,
            seconds_streamed,
            total_fed_bytes: total_fed,
            pcm_byte_rate: 176_400,
            frame_size: 4,
            ring,
            ring_capacity_bytes: 6 * 1024 * 1024,
        }
    }

    #[test]
    fn start_happens_before_prime() {
        let ring = vec![7u8; 352_800];
        let pre = snapshot(&ring, 20_000, 10_000, 12.0, 2_116_800);
        let request = msa_prepare_late_join(&pre, 0, 0).unwrap();

        let mut rt = FakeRuntime {
            adjust: 0,
            start_ack: request.commanded_member_anchor_unix_ms,
            calls: Vec::new(),
            rebased: None,
            prime_len: 0,
            skip: 0,
        };

        let ack = msa_start_late_join(&mut rt, &request).unwrap();
        assert_eq!(rt.calls, vec!["start"]);

        let post = snapshot(&ring, 20_100, 10_000, 12.1, 2_134_440);
        let _ = msa_commit_late_join(&mut rt, &post, 0, ack).unwrap();

        assert_eq!(rt.calls[0], "start");
        assert!(rt.calls.iter().position(|c| *c == "prime").unwrap() > 0);
    }

    #[test]
    fn ack_correction_is_normalized_by_sync_adjust() {
        let ring = vec![7u8; 2_116_800];
        let pre = snapshot(&ring, 20_000, 10_000, 12.0, 2_116_800);
        let request = msa_prepare_late_join(&pre, 5_000, 25).unwrap();

        let mut rt = FakeRuntime {
            adjust: 25,
            start_ack: request.commanded_member_anchor_unix_ms + 200,
            calls: Vec::new(),
            rebased: None,
            prime_len: 0,
            skip: 0,
        };

        let ack = msa_start_late_join(&mut rt, &request).unwrap();
        let post = snapshot(&ring, 20_200, 10_000, 12.2, 2_152_080);
        let result = msa_commit_late_join(&mut rt, &post, 5_000, ack).unwrap();

        assert_eq!(
            result.actual_group_anchor_unix_ms,
            ack - 25
        );
        assert_eq!(rt.rebased, Some(result.position_ms));
    }

    #[test]
    fn existing_group_is_not_part_of_late_join_contract() {
        // This API accepts only the joiner runtime and live timeline snapshots.
        // There is deliberately no group-member START surface to call.
        let ring = vec![7u8; 352_800];
        let pre = snapshot(&ring, 20_000, 10_000, 12.0, 2_116_800);
        let request = msa_prepare_late_join(&pre, 0, 0).unwrap();

        assert!(request.commanded_member_anchor_unix_ms >= 22_500);
    }

    #[test]
    fn due_ahead_sets_joiner_only_live_skip_after_ack() {
        let ring = vec![7u8; 352_800];
        let pre = snapshot(&ring, 20_000, 10_000, 10.0, 1_764_000);
        let request = msa_prepare_late_join(&pre, 0, 0).unwrap();

        let mut rt = FakeRuntime {
            adjust: 0,
            start_ack: request.commanded_member_anchor_unix_ms,
            calls: Vec::new(),
            rebased: None,
            prime_len: 0,
            skip: 0,
        };

        let ack = msa_start_late_join(&mut rt, &request).unwrap();
        let post = snapshot(&ring, 20_100, 10_000, 10.1, 1_781_640);
        let result = msa_commit_late_join(&mut rt, &post, 0, ack).unwrap();

        assert_eq!(rt.skip, result.committed_plan.skip_live_bytes);
        assert!(rt.calls.contains(&"skip"));
    }
}
