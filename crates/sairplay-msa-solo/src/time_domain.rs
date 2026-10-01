//! Compile-time time-domain boundaries for the independent MSA SOLO engine.
//!
//! Pinned source contracts:
//! - music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128
//! - its libraop submodule @ 81c2182649da8645ac2a58b78e9f370c79a4165b
//!
//! IMPORTANT: pinned libraop raopcl_get_ntp(NULL) is NOT RFC/NTP epoch 1900.
//! It packs gettime_us() directly as seconds<<32 | fraction. MSA AP2 scheduling
//! therefore uses a Unix/system-wall fixed-point domain. The AirPlay NTP timing
//! responder is a different protocol domain and uses RFC/NTP epoch 1900.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub const RFC_NTP_UNIX_EPOCH_DELTA_SECS: u64 = 2_208_988_800;
const FIXED32_SCALE: u128 = 1u128 << 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeDomainError {
    BeforeUnixEpoch,
    SecondsOutOfRange,
}

impl fmt::Display for TimeDomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeUnixEpoch => f.write_str("system time is before the Unix epoch"),
            Self::SecondsOutOfRange => f.write_str("fixed-point seconds exceed the 32-bit source domain"),
        }
    }
}

/// MSA/libraop scheduling time: 32.32 fixed point, seconds in the Unix/system
/// wall-clock domain used by pinned raopcl_get_ntp(NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SourceNtp(u64);

impl SourceNtp {
    pub const ZERO: Self = Self(0);

    /// Explicit raw constructor. Keep raw ingress visually obvious at protocol
    /// boundaries; never use this to convert RFC-NTP.
    pub const fn from_raw(raw: u64) -> Self { Self(raw) }
    pub const fn raw(self) -> u64 { self.0 }

    pub fn from_system_time(time: SystemTime) -> Result<Self, TimeDomainError> {
        let d = time.duration_since(UNIX_EPOCH).map_err(|_| TimeDomainError::BeforeUnixEpoch)?;
        if d.as_secs() > u64::from(u32::MAX) {
            return Err(TimeDomainError::SecondsOutOfRange);
        }
        let fraction = ((u128::from(d.subsec_nanos()) << 32) / 1_000_000_000u128) as u64;
        Ok(Self((d.as_secs() << 32) | fraction))
    }

    pub fn now() -> Result<Self, TimeDomainError> {
        Self::from_system_time(SystemTime::now())
    }

    pub fn from_unix_ms(ms: u64) -> Self {
        let seconds = ms / 1000;
        let fraction = ((u128::from(ms % 1000) << 32) / 1000) as u64;
        Self((seconds << 32) | fraction)
    }

    pub fn to_unix_ms(self) -> u64 {
        ((u128::from(self.0 >> 32) * 1000)
            + ((u128::from(self.0 & 0xffff_ffff) * 1000) >> 32)) as u64
    }

    pub fn to_unix_ns(self) -> u64 {
        (self.0 >> 32)
            .saturating_mul(1_000_000_000)
            .saturating_add(
                (((self.0 & 0xffff_ffff) as u128 * 1_000_000_000u128) >> 32) as u64,
            )
    }

    pub fn add_ms(self, ms: u64) -> Self {
        let delta = ((u128::from(ms) * FIXED32_SCALE) / 1000)
            .min(u128::from(u64::MAX)) as u64;
        Self(self.0.saturating_add(delta))
    }

    pub fn to_frames(self, sample_rate: u32) -> u64 {
        if sample_rate == 0 {
            0
        } else {
            ((u128::from(self.0) * u128::from(sample_rate)) >> 32) as u64
        }
    }

    pub fn duration_ns_since(self, earlier: Self) -> u64 {
        let d = self.0.saturating_sub(earlier.0);
        (d >> 32)
            .saturating_mul(1_000_000_000)
            .saturating_add((((d & 0xffff_ffff) as u128 * 1_000_000_000u128) >> 32) as u64)
    }
}

/// RFC/NTP protocol timestamp: 32.32 fixed point, epoch 1900.
/// This type is intentionally distinct from SourceNtp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RfcNtp(u64);

impl RfcNtp {
    pub const fn from_raw(raw: u64) -> Self { Self(raw) }
    pub const fn raw(self) -> u64 { self.0 }

    pub fn from_system_time(time: SystemTime) -> Result<Self, TimeDomainError> {
        let d = time.duration_since(UNIX_EPOCH).map_err(|_| TimeDomainError::BeforeUnixEpoch)?;
        let seconds = d
            .as_secs()
            .checked_add(RFC_NTP_UNIX_EPOCH_DELTA_SECS)
            .ok_or(TimeDomainError::SecondsOutOfRange)?;
        if seconds > u64::from(u32::MAX) {
            return Err(TimeDomainError::SecondsOutOfRange);
        }
        let fraction = ((u128::from(d.subsec_nanos()) << 32) / 1_000_000_000u128) as u64;
        Ok(Self((seconds << 32) | fraction))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImmediateStartViolation {
    pub requested_unix_ms: u64,
    pub accepted_unix_ms: u64,
    pub delta_ms: u64,
    pub max_delta_ms: u64,
}

impl fmt::Display for ImmediateStartViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requested={} accepted={} delta={}ms exceeds immediate-start bound {}ms",
            self.requested_unix_ms, self.accepted_unix_ms, self.delta_ms, self.max_delta_ms
        )
    }
}

/// GUI/hardware fail-closed guard. An immediate START is allowed a bounded
/// correction for PTP clock seating, but an epoch-sized jump is never legal.
pub fn validate_immediate_start(
    requested_unix_ms: u64,
    accepted_unix_ms: u64,
    max_delta_ms: u64,
) -> Result<(), ImmediateStartViolation> {
    let delta_ms = requested_unix_ms.abs_diff(accepted_unix_ms);
    if accepted_unix_ms == 0 || delta_ms > max_delta_ms {
        return Err(ImmediateStartViolation {
            requested_unix_ms,
            accepted_unix_ms,
            delta_ms,
            max_delta_ms,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn source_and_rfc_ntp_are_distinct_by_exact_epoch_offset() {
        let t = UNIX_EPOCH + Duration::from_secs(1_790_827_933) + Duration::from_millis(506);
        let source = SourceNtp::from_system_time(t).unwrap();
        let rfc = RfcNtp::from_system_time(t).unwrap();
        assert_eq!((rfc.raw() >> 32) - (source.raw() >> 32), RFC_NTP_UNIX_EPOCH_DELTA_SECS);
        assert_eq!(source.to_unix_ms(), 1_790_827_933_505);
    }

    #[test]
    fn source_ntp_matches_pinned_libraop_gettime_shape() {
        assert_eq!(SourceNtp::from_system_time(UNIX_EPOCH).unwrap(), SourceNtp::ZERO);
        assert_eq!(
            SourceNtp::from_system_time(UNIX_EPOCH + Duration::from_secs(1)).unwrap().raw(),
            1u64 << 32
        );
    }

    #[test]
    fn immediate_start_guard_rejects_the_exact_epoch_class_failure() {
        let requested = 1_790_827_933_506u64;
        let accepted = requested + RFC_NTP_UNIX_EPOCH_DELTA_SECS * 1000 + 500;
        let error = validate_immediate_start(requested, accepted, 10_000).unwrap_err();
        assert!(error.delta_ms > 2_000_000_000_000);
    }

    #[test]
    fn immediate_start_guard_allows_ptp_seating_correction() {
        assert!(validate_immediate_start(1_000_000, 1_002_800, 10_000).is_ok());
    }
}
