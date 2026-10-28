//! HappyCast-only wire release adapter; enabled by the explicit NTP test endpoint.
use std::time::{Duration, Instant};

const MIN_GAP: Duration = Duration::from_millis(7);

#[derive(Default)]
pub(super) struct MiTvMediaPacer {
    last: Option<Instant>,
}

impl MiTvMediaPacer {
    fn deadline(&self, now: Instant) -> Instant {
        self.last.map_or(now, |last| now.max(last + MIN_GAP))
    }

    pub(super) fn wait(&self) {
        let deadline = self.deadline(Instant::now());
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() { break; }
            std::thread::sleep(remaining);
        }
    }

    pub(super) fn committed(&mut self, now: Instant) {
        // Only bound close releases. MSA's existing window owns average rate
        // and must retain enough catch-up capacity to consume recovery padding.
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_gap_retains_capacity_to_drain_recovery_padding() {
        let start = Instant::now();
        let mut p = MiTvMediaPacer::default();
        assert_eq!(p.deadline(start), start);
        p.committed(start);
        assert_eq!(p.deadline(start), start + MIN_GAP);
        let mut now = start;
        for _ in 0..100 {
            now = p.deadline(now);
            p.committed(now);
        }
        // A fixed 352/44100 cadence cannot repay a timeline deficit: each
        // silence packet only replaces the wall time spent sending it.
        let produced = Duration::from_nanos(100 * 352 * 1_000_000_000 / 44_100);
        assert!(now.duration_since(start) < produced);
        let stalled = start + Duration::from_secs(1);
        assert_eq!(p.deadline(stalled), stalled);
        p.committed(stalled);
        assert_eq!(p.deadline(stalled), stalled + MIN_GAP);
    }
}
