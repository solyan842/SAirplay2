//! Music Assistant server-style late-join timeline mapping.
//!
//! Source: pinned `AirPlayStreamSession.add_client()`.
//! This module contains only deterministic timeline/ring math. Runtime connect,
//! clock readiness, START command delivery and live-feed writes stay outside.

pub const MSA_LATE_JOIN_MIN_HEADROOM_MS: u64 = 2500;
pub const MSA_LATE_JOIN_RING_MIN_SECONDS: f64 = 12.0;
pub const MSA_LATE_JOIN_RING_MARGIN_SECONDS: f64 = 2.0;
pub const MSA_LATE_JOIN_RING_MAX_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct MsaLateJoinSnapshot<'a> {
    pub now_unix_ms: u64,
    pub effective_start_unix_ms: u64,
    pub ready_at_unix_ms: u64,
    pub seconds_streamed: f64,
    pub total_fed_bytes: u64,
    pub pcm_byte_rate: u64,
    pub frame_size: usize,
    pub ring: &'a [u8],
    pub ring_capacity_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaLateJoinPlan {
    pub requested_start_unix_ms: u64,
    pub due_position_bytes: u64,
    pub prime: Vec<u8>,
    pub skip_live_bytes: u64,
    pub moved_anchor_forward_ms: u64,
    pub padded_missing_bytes: usize,
    pub residual_missing_bytes: usize,
}

pub fn msa_late_join_ring_capacity_bytes(byte_rate: u64, peak_lead_seconds: f64) -> usize {
    if byte_rate == 0 {
        return 0;
    }
    let seconds = MSA_LATE_JOIN_RING_MIN_SECONDS
        .max(peak_lead_seconds + MSA_LATE_JOIN_RING_MARGIN_SECONDS);
    let requested = (seconds * byte_rate as f64).ceil() as usize;
    requested.min(MSA_LATE_JOIN_RING_MAX_BYTES)
}

pub fn msa_late_join_requested_anchor_unix_ms(
    now_unix_ms: u64,
    ready_at_unix_ms: u64,
    clock_ready_lead_ms: u64,
) -> u64 {
    let mut anchor = now_unix_ms.saturating_add(MSA_LATE_JOIN_MIN_HEADROOM_MS);
    if ready_at_unix_ms != 0 {
        anchor = anchor.max(ready_at_unix_ms.saturating_add(clock_ready_lead_ms));
    }
    anchor
}

fn align_up(value: u64, align: usize) -> u64 {
    if align <= 1 {
        return value;
    }
    let a = align as u64;
    value.saturating_add((a - value % a) % a)
}

pub fn msa_plan_late_join(
    snapshot: &MsaLateJoinSnapshot<'_>,
    requested_anchor_unix_ms: u64,
    committed: bool,
) -> Result<MsaLateJoinPlan, String> {
    if snapshot.pcm_byte_rate == 0 {
        return Err("late join requires non-zero PCM byte rate".into());
    }
    if snapshot.frame_size == 0 {
        return Err("late join requires non-zero frame size".into());
    }
    if requested_anchor_unix_ms < snapshot.effective_start_unix_ms {
        return Err("late join anchor predates effective group timeline".into());
    }

    let due_ms = requested_anchor_unix_ms - snapshot.effective_start_unix_ms;
    let due_position_bytes =
        ((due_ms as u128 * snapshot.pcm_byte_rate as u128) / 1000) as u64;
    let write_head_bytes =
        (snapshot.seconds_streamed * snapshot.pcm_byte_rate as f64).round() as u64;

    if due_position_bytes <= write_head_bytes {
        let mut keep_bytes = write_head_bytes - due_position_bytes;

        // MSA aligns the prime start in absolute stream coordinates while the
        // prime still ends exactly on the current write head.
        let start_abs = snapshot.total_fed_bytes.saturating_sub(keep_bytes);
        let aligned_start_abs = align_up(start_abs, snapshot.frame_size);
        let realign = aligned_start_abs.saturating_sub(start_abs);
        keep_bytes = keep_bytes.saturating_sub(realign);

        let ring_len = snapshot.ring.len() as u64;
        let ring_head_abs = snapshot.total_fed_bytes.saturating_sub(ring_len);
        let ring_aligned_abs = align_up(ring_head_abs, snapshot.frame_size);
        let ring_realign = ring_aligned_abs.saturating_sub(ring_head_abs) as usize;
        let servable = if ring_realign >= snapshot.ring.len() {
            &[][..]
        } else {
            &snapshot.ring[ring_realign..]
        };

        if keep_bytes <= servable.len() as u64 {
            let n = keep_bytes as usize;
            let prime = if n == 0 {
                Vec::new()
            } else {
                servable[servable.len() - n..].to_vec()
            };
            return Ok(MsaLateJoinPlan {
                requested_start_unix_ms: requested_anchor_unix_ms,
                due_position_bytes,
                prime,
                skip_live_bytes: 0,
                moved_anchor_forward_ms: 0,
                padded_missing_bytes: 0,
                residual_missing_bytes: 0,
            });
        }

        let missing = keep_bytes.saturating_sub(servable.len() as u64) as usize;
        if committed {
            // After ACK the instant belongs to the binary and cannot move.
            // MSA pads the missing head with silence, bounded by the ring cap.
            let pad = missing.min(snapshot.ring_capacity_bytes);
            let mut prime = vec![0u8; pad];
            prime.extend_from_slice(servable);
            return Ok(MsaLateJoinPlan {
                requested_start_unix_ms: requested_anchor_unix_ms,
                due_position_bytes,
                prime,
                skip_live_bytes: 0,
                moved_anchor_forward_ms: 0,
                padded_missing_bytes: pad,
                residual_missing_bytes: missing.saturating_sub(pad),
            });
        }

        // Before START is committed, MSA moves the free anchor forward to the
        // oldest sample still held in the ring rather than inventing silence.
        let missing_ms =
            ((missing as u128 * 1000) / snapshot.pcm_byte_rate as u128) as u64;
        return Ok(MsaLateJoinPlan {
            requested_start_unix_ms: requested_anchor_unix_ms.saturating_add(missing_ms),
            due_position_bytes: due_position_bytes.saturating_add(missing as u64),
            prime: servable.to_vec(),
            skip_live_bytes: 0,
            moved_anchor_forward_ms: missing_ms,
            padded_missing_bytes: 0,
            residual_missing_bytes: 0,
        });
    }

    // The due sample is ahead of the write head. Skip the corresponding head
    // of the live feed, aligned in absolute stream coordinates.
    let skip = due_position_bytes - write_head_bytes;
    let first_abs = snapshot.total_fed_bytes.saturating_add(skip);
    let aligned_first_abs = align_up(first_abs, snapshot.frame_size);
    let aligned_skip = skip.saturating_add(aligned_first_abs - first_abs);

    Ok(MsaLateJoinPlan {
        requested_start_unix_ms: requested_anchor_unix_ms,
        due_position_bytes,
        prime: Vec::new(),
        skip_live_bytes: aligned_skip,
        moved_anchor_forward_ms: 0,
        padded_missing_bytes: 0,
        residual_missing_bytes: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot<'a>(ring: &'a [u8]) -> MsaLateJoinSnapshot<'a> {
        MsaLateJoinSnapshot {
            now_unix_ms: 20_000,
            effective_start_unix_ms: 10_000,
            ready_at_unix_ms: 0,
            seconds_streamed: 10.0,
            total_fed_bytes: 1_764_000,
            pcm_byte_rate: 176_400,
            frame_size: 4,
            ring,
            ring_capacity_bytes: 6 * 1024 * 1024,
        }
    }

    #[test]
    fn late_join_anchor_obeys_headroom_and_clock_projection() {
        assert_eq!(
            msa_late_join_requested_anchor_unix_ms(20_000, 0, 500),
            22_500
        );
        assert_eq!(
            msa_late_join_requested_anchor_unix_ms(20_000, 23_000, 500),
            23_500
        );
    }

    #[test]
    fn ring_capacity_matches_msa_floor_growth_and_byte_cap() {
        assert_eq!(
            msa_late_join_ring_capacity_bytes(176_400, 0.0),
            (12.0 * 176_400.0) as usize
        );
        assert_eq!(
            msa_late_join_ring_capacity_bytes(176_400, 15.0),
            (17.0 * 176_400.0) as usize
        );
        assert_eq!(
            msa_late_join_ring_capacity_bytes(1_000_000, 20.0),
            MSA_LATE_JOIN_RING_MAX_BYTES
        );
    }

    #[test]
    fn due_behind_write_head_primes_from_ring_tail() {
        let ring = vec![7u8; 352_800]; // 2 seconds
        let s = snapshot(&ring);
        // Anchor at 19s => due=9s, while write head=10s: prime one second.
        let plan = msa_plan_late_join(&s, 19_000, false).unwrap();
        assert_eq!(plan.prime.len(), 176_400);
        assert_eq!(plan.skip_live_bytes, 0);
        assert_eq!(plan.requested_start_unix_ms, 19_000);
    }

    #[test]
    fn due_ahead_of_write_head_skips_live_feed() {
        let ring = vec![7u8; 352_800];
        let s = snapshot(&ring);
        // Anchor at 20.5s => due=10.5s, write head=10s: skip 0.5s.
        let plan = msa_plan_late_join(&s, 20_500, false).unwrap();
        assert_eq!(plan.prime.len(), 0);
        assert_eq!(plan.skip_live_bytes, 88_200);
    }

    #[test]
    fn uncommitted_anchor_moves_forward_when_ring_is_too_short() {
        let ring = vec![7u8; 88_200]; // 0.5 second only
        let s = snapshot(&ring);
        // Due=8s, write head=10s requires 2s; 1.5s is missing.
        let plan = msa_plan_late_join(&s, 18_000, false).unwrap();
        assert_eq!(plan.prime.len(), 88_200);
        assert_eq!(plan.moved_anchor_forward_ms, 1500);
        assert_eq!(plan.requested_start_unix_ms, 19_500);
    }

    #[test]
    fn committed_anchor_pads_missing_head_instead_of_moving() {
        let ring = vec![7u8; 88_200];
        let mut s = snapshot(&ring);
        s.ring_capacity_bytes = 352_800;
        let plan = msa_plan_late_join(&s, 18_000, true).unwrap();
        assert_eq!(plan.requested_start_unix_ms, 18_000);
        assert_eq!(plan.padded_missing_bytes, 264_600);
        assert_eq!(plan.residual_missing_bytes, 0);
        assert_eq!(plan.prime.len(), 352_800);
    }
}
