//! Receiver-clock readiness projection for the SAirplay 2.0 MSA clone.
//!
//! This lifts the already-measured PTP exchange telemetry into the explicit
//! readiness contract used by pinned Music Assistant. START planning must use
//! this projection rather than a fixed Windows cold-start delay.

use crate::PtpExchange;

pub const MSA_AP2_CLOCK_LOCK_MS: u64 = 2300;
pub const MSA_AP2_CLOCK_SETTLE_MS: u64 = 250;
pub const MSA_AP2_CLOCK_SEAT_EXCHANGES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaAp2ClockState {
    Cold,
    Probing,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaAp2ClockReadiness {
    pub state: MsaAp2ClockState,
    pub exchanges: u32,
    pub ready_in_ms: u64,
    pub ready_at_unix_ms: u64,
}

pub fn msa_ap2_clock_readiness(
    now_unix_ms: u64,
    exchange: Option<PtpExchange>,
    apple_model: bool,
) -> MsaAp2ClockReadiness {
    let Some(exchange) = exchange else {
        return MsaAp2ClockReadiness {
            state: MsaAp2ClockState::Cold,
            exchanges: 0,
            ready_in_ms: MSA_AP2_CLOCK_LOCK_MS,
            ready_at_unix_ms: now_unix_ms.saturating_add(MSA_AP2_CLOCK_LOCK_MS),
        };
    };

    let full_remaining = MSA_AP2_CLOCK_LOCK_MS.saturating_sub(exchange.first_ms);
    let ready_in_ms = if apple_model && exchange.count >= MSA_AP2_CLOCK_SEAT_EXCHANGES {
        full_remaining.min(
            MSA_AP2_CLOCK_SETTLE_MS.saturating_sub(exchange.third_ms),
        )
    } else {
        full_remaining
    };

    MsaAp2ClockReadiness {
        state: if ready_in_ms == 0 {
            MsaAp2ClockState::Ready
        } else {
            MsaAp2ClockState::Probing
        },
        exchanges: exchange.count,
        ready_in_ms,
        ready_at_unix_ms: now_unix_ms.saturating_add(ready_in_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_clock_projects_full_lock_window() {
        let r = msa_ap2_clock_readiness(100_000, None, true);
        assert_eq!(r.state, MsaAp2ClockState::Cold);
        assert_eq!(r.exchanges, 0);
        assert_eq!(r.ready_in_ms, 2300);
        assert_eq!(r.ready_at_unix_ms, 102_300);
    }

    #[test]
    fn first_exchange_uses_remaining_full_lock() {
        let r = msa_ap2_clock_readiness(
            100_000,
            Some(PtpExchange {
                count: 1,
                first_ms: 900,
                last_ms: 0,
                third_ms: 0,
            }),
            true,
        );
        assert_eq!(r.state, MsaAp2ClockState::Probing);
        assert_eq!(r.ready_in_ms, 1400);
        assert_eq!(r.ready_at_unix_ms, 101_400);
    }

    #[test]
    fn apple_third_exchange_uses_fast_seat_window() {
        let r = msa_ap2_clock_readiness(
            100_000,
            Some(PtpExchange {
                count: 3,
                first_ms: 1200,
                last_ms: 0,
                third_ms: 100,
            }),
            true,
        );
        assert_eq!(r.ready_in_ms, 150);
        assert_eq!(r.ready_at_unix_ms, 100_150);
    }

    #[test]
    fn non_apple_does_not_take_fast_seat_path() {
        let r = msa_ap2_clock_readiness(
            100_000,
            Some(PtpExchange {
                count: 3,
                first_ms: 1200,
                last_ms: 0,
                third_ms: 100,
            }),
            false,
        );
        assert_eq!(r.ready_in_ms, 1100);
    }

    #[test]
    fn settled_clock_reports_ready_now() {
        let r = msa_ap2_clock_readiness(
            100_000,
            Some(PtpExchange {
                count: 3,
                first_ms: 2400,
                last_ms: 0,
                third_ms: 300,
            }),
            true,
        );
        assert_eq!(r.state, MsaAp2ClockState::Ready);
        assert_eq!(r.ready_in_ms, 0);
        assert_eq!(r.ready_at_unix_ms, 100_000);
    }
}
