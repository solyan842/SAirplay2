//! HappyCast-only wire release adapter; enabled by the explicit NTP test endpoint.
use std::time::{Duration, Instant};

const PERIOD: Duration = Duration::from_nanos(352 * 1_000_000_000 / 44_100);
const MIN_GAP: Duration = Duration::from_millis(7);

#[derive(Default)]
pub(super) struct MiTvMediaPacer {
    next: Option<Instant>,
    last: Option<Instant>,
}

impl MiTvMediaPacer {
    fn deadline(&self, now: Instant) -> Instant {
        let nominal = self.next.unwrap_or(now);
        self.last.map_or(nominal, |last| nominal.max(last + MIN_GAP))
    }

    pub(super) fn wait(&self) {
        let deadline = self.deadline(Instant::now());
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() { break; }
            std::thread::sleep(remaining);
        }
    }

    pub(super) fn committed(&mut self, now: Instant) {
        // Absolute deadlines retain the stream rate despite ordinary wake jitter.
        // After a stall, the minimum gap prevents a back-to-back catch-up burst.
        self.next = Some((self.next.unwrap_or(now) + PERIOD).max(now));
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_preserve_rate_and_bound_catch_up() {
        let start = Instant::now();
        let mut p = MiTvMediaPacer::default();
        assert_eq!(p.deadline(start), start);
        p.committed(start);
        let jittered = start + PERIOD + Duration::from_micros(500);
        p.committed(jittered);
        assert_eq!(p.deadline(jittered), start + PERIOD * 2);
        let stalled = start + Duration::from_secs(1);
        p.committed(stalled);
        assert_eq!(p.deadline(stalled), stalled + MIN_GAP);
    }
}
