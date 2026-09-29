//! Audible START feasibility contracts from pinned MSA source.

pub const RAOP_MIN_START_LEAD_MS: u64 = 200;
pub const AP2_MIN_WARM_LEAD_MS: u64 = 250;
pub const AP2_CLOCK_LOCK_MS: u64 = 2300;
pub const AP2_CLOCK_SETTLE_MS: u64 = 250;
pub const AP2_CLOCK_SEAT_EXCHANGES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartResolution {
    pub requested_unix_ms: u64,
    pub at_unix_ms: u64,
    pub corrected_forward: bool,
}

/// MSA RAOP contract: feasible non-zero requests are exact; an infeasible
/// request is moved to floor + one lead of retry slack; zero selects floor.
pub fn resolve_raop_start(now_unix_ms: u64, requested_unix_ms: u64) -> StartResolution {
    resolve_with_lead(now_unix_ms, requested_unix_ms, RAOP_MIN_START_LEAD_MS)
}

/// Native AP2 uses the same commanded/verified shape with its 250ms warm floor.
/// Receiver clock readiness is layered by the native transport when applicable.
pub fn resolve_ap2_warm_start(now_unix_ms: u64, requested_unix_ms: u64) -> StartResolution {
    resolve_with_lead(now_unix_ms, requested_unix_ms, AP2_MIN_WARM_LEAD_MS)
}

fn resolve_with_lead(now: u64, requested: u64, lead: u64) -> StartResolution {
    let floor=now.saturating_add(lead);
    if requested == 0 {
        return StartResolution { requested_unix_ms: 0, at_unix_ms: floor, corrected_forward: false };
    }
    if requested >= floor {
        return StartResolution { requested_unix_ms: requested, at_unix_ms: requested, corrected_forward: false };
    }
    StartResolution { requested_unix_ms: requested, at_unix_ms: floor.saturating_add(lead), corrected_forward: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn raop_exact_when_feasible() { assert_eq!(resolve_raop_start(1000,1500).at_unix_ms,1500); }
    #[test] fn raop_corrects_forward_with_retry_slack() { assert_eq!(resolve_raop_start(1000,1100).at_unix_ms,1400); }
    #[test] fn zero_means_earliest_floor() { assert_eq!(resolve_ap2_warm_start(1000,0).at_unix_ms,1250); }
}
