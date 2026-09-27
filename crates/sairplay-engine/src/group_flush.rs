//! Transport-neutral FLUSH acknowledgement shape.
//!
//! Pinned airplay-cli emits:
//!   [STATUS] flushed [head_unix_ms=<ms>]
//!
//! The optional head is the audible instant frozen by the warm boundary.
//! Absence (or zero) means the transport imposes no warm-anchor constraint.
//! This module deliberately does not decide when continuous WASAPI should
//! flush; it only normalizes the acknowledgement contract.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupFlushAck {
    pub head_unix_ms: Option<u64>,
}

impl GroupFlushAck {
    pub const fn no_head_constraint() -> Self {
        Self { head_unix_ms: None }
    }

    pub const fn with_head(head_unix_ms: u64) -> Self {
        Self {
            head_unix_ms: if head_unix_ms == 0 {
                None
            } else {
                Some(head_unix_ms)
            },
        }
    }
}

/// Parse the exact persistent-session FLUSH status emitted by pinned
/// airplay-cli. Non-FLUSH status lines are ignored.
pub fn parse_group_flush_status(line: &str) -> Option<GroupFlushAck> {
    if !line.starts_with("[STATUS] flushed") {
        return None;
    }

    let head_unix_ms = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("head_unix_ms="))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value != 0);

    Some(GroupFlushAck { head_unix_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raop_flush_without_head_constraint() {
        assert_eq!(
            parse_group_flush_status("[STATUS] flushed"),
            Some(GroupFlushAck::no_head_constraint())
        );
    }

    #[test]
    fn parses_splice_flush_with_frozen_head() {
        assert_eq!(
            parse_group_flush_status("[STATUS] flushed head_unix_ms=12345"),
            Some(GroupFlushAck::with_head(12345))
        );
    }

    #[test]
    fn zero_head_normalizes_to_no_constraint() {
        assert_eq!(
            parse_group_flush_status("[STATUS] flushed head_unix_ms=0"),
            Some(GroupFlushAck::no_head_constraint())
        );
    }

    #[test]
    fn ignores_other_status_lines() {
        assert_eq!(
            parse_group_flush_status(
                "[STATUS] started requested_unix_ms=1 at_unix_ms=1"
            ),
            None
        );
    }
}
