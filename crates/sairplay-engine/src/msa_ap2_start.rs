//! AP2 START feasibility/ack resolution for the SAirplay 2.0 MSA clone.
//!
//! Mirrors pinned `ap2_clock_floor()` + `ap2_resolve_start()`:
//! - base feasibility floor = now + 250 ms;
//! - a live receiver-clock projection may raise that floor;
//! - a feasible nonzero request is honored exactly;
//! - an infeasible nonzero request is corrected to floor + 250 ms so a retry
//!   does not chase a moving wall-clock floor forever;
//! - request=0 selects the floor directly.
//! The returned audible instant is the START ACK truth.

use crate::{MsaAp2ClockReadiness, MsaAp2ClockState};

pub const MSA_AP2_MIN_WARM_LEAD_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaAp2StartResolution {
    pub requested_unix_ms: u64,
    pub feasibility_floor_unix_ms: u64,
    pub acknowledged_unix_ms: u64,
    pub corrected: bool,
    pub clock_cold: bool,
}

pub fn msa_ap2_start_floor_unix_ms(
    now_unix_ms: u64,
    clock: MsaAp2ClockReadiness,
) -> (u64, bool) {
    let base = now_unix_ms.saturating_add(MSA_AP2_MIN_WARM_LEAD_MS);

    // Pinned ap2_clock_floor() does NOT raise the floor when no live streak
    // exists. It marks the clock cold and lets post-commit verification handle
    // that case. A caller that waited for readiness will normally arrive here
    // with Probing/Ready instead.
    if clock.state == MsaAp2ClockState::Cold {
        return (base, true);
    }

    (base.max(clock.ready_at_unix_ms), false)
}

pub fn msa_resolve_ap2_start(
    now_unix_ms: u64,
    requested_unix_ms: u64,
    clock: MsaAp2ClockReadiness,
) -> MsaAp2StartResolution {
    let (floor, clock_cold) = msa_ap2_start_floor_unix_ms(now_unix_ms, clock);

    let acknowledged = if requested_unix_ms == 0 {
        floor
    } else if requested_unix_ms >= floor {
        requested_unix_ms
    } else {
        floor.saturating_add(MSA_AP2_MIN_WARM_LEAD_MS)
    };

    MsaAp2StartResolution {
        requested_unix_ms,
        feasibility_floor_unix_ms: floor,
        acknowledged_unix_ms: acknowledged,
        corrected: requested_unix_ms != 0 && acknowledged != requested_unix_ms,
        clock_cold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(state: MsaAp2ClockState, ready_at: u64) -> MsaAp2ClockReadiness {
        MsaAp2ClockReadiness {
            state,
            exchanges: 3,
            ready_in_ms: ready_at.saturating_sub(10_000),
            ready_at_unix_ms: ready_at,
        }
    }

    #[test]
    fn feasible_nonzero_request_is_honored_exactly() {
        let r = msa_resolve_ap2_start(
            10_000,
            11_000,
            clock(MsaAp2ClockState::Ready, 10_000),
        );
        assert_eq!(r.feasibility_floor_unix_ms, 10_250);
        assert_eq!(r.acknowledged_unix_ms, 11_000);
        assert!(!r.corrected);
    }

    #[test]
    fn zero_request_takes_floor_without_retry_slack() {
        let r = msa_resolve_ap2_start(
            10_000,
            0,
            clock(MsaAp2ClockState::Ready, 10_000),
        );
        assert_eq!(r.acknowledged_unix_ms, 10_250);
        assert!(!r.corrected);
    }

    #[test]
    fn infeasible_nonzero_request_gets_floor_plus_one_warm_lead() {
        let r = msa_resolve_ap2_start(
            10_000,
            10_100,
            clock(MsaAp2ClockState::Ready, 10_000),
        );
        assert_eq!(r.feasibility_floor_unix_ms, 10_250);
        assert_eq!(r.acknowledged_unix_ms, 10_500);
        assert!(r.corrected);
    }

    #[test]
    fn probing_clock_can_raise_feasibility_floor() {
        let r = msa_resolve_ap2_start(
            10_000,
            10_500,
            clock(MsaAp2ClockState::Probing, 11_400),
        );
        assert_eq!(r.feasibility_floor_unix_ms, 11_400);
        assert_eq!(r.acknowledged_unix_ms, 11_650);
        assert!(r.corrected);
    }

    #[test]
    fn cold_clock_leaves_floor_at_now_plus_warm_lead_and_marks_verification_needed() {
        let r = msa_resolve_ap2_start(
            10_000,
            10_500,
            MsaAp2ClockReadiness {
                state: MsaAp2ClockState::Cold,
                exchanges: 0,
                ready_in_ms: 2300,
                ready_at_unix_ms: 12_300,
            },
        );
        assert_eq!(r.feasibility_floor_unix_ms, 10_250);
        assert_eq!(r.acknowledged_unix_ms, 10_500);
        assert!(r.clock_cold);
    }
}
