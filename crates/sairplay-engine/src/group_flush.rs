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

/// One member's warm-start constraints after its FLUSH acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmGroupMemberConstraint {
    pub sync_adjust_ms: i64,
    /// Transport-declared warm lead requirement. Zero means no extra lead.
    pub warm_lead_ms: u64,
    pub flush_ack: GroupFlushAck,
}

/// Apply current Music Assistant warm-group anchor planning.
///
/// `base_anchor_unix_ms` is the caller's already-planned warm anchor (for a
/// group, normally now + AIRPLAY_GROUP_START_LEAD_MS, plus any clock-readiness
/// floor). This function only adds transport-specific warm constraints:
///
/// - negative sync_adjust consumes warm lead, so it increases the requirement;
/// - a frozen FLUSH head must be cleared by the member's adjusted START;
/// - every such transport constraint gets AIRPLAY_SPLICE_LEAD_MARGIN_MS.
///
/// No I/O occurs here and no FLUSH is triggered.
pub fn resolve_warm_group_anchor(
    now_unix_ms: u64,
    base_anchor_unix_ms: u64,
    members: &[WarmGroupMemberConstraint],
) -> u64 {
    use crate::cross_transport_timeline::AIRPLAY_SPLICE_LEAD_MARGIN_MS;

    let mut anchor = base_anchor_unix_ms;

    let mut member_requirement_ms = 0u64;
    for member in members {
        if member.warm_lead_ms == 0 {
            continue;
        }
        let negative_adjust_ms = if member.sync_adjust_ms < 0 {
            member.sync_adjust_ms.unsigned_abs()
        } else {
            0
        };
        member_requirement_ms = member_requirement_ms.max(
            member
                .warm_lead_ms
                .saturating_add(negative_adjust_ms),
        );
    }

    if member_requirement_ms > 0 {
        anchor = anchor.max(
            now_unix_ms
                .saturating_add(member_requirement_ms)
                .saturating_add(AIRPLAY_SPLICE_LEAD_MARGIN_MS),
        );
    }

    for member in members {
        let Some(head_unix_ms) = member.flush_ack.head_unix_ms else {
            continue;
        };

        // MSA constraint: anchor >= head - sync_adjust + margin.
        let unadjusted_head = if member.sync_adjust_ms >= 0 {
            head_unix_ms.saturating_sub(member.sync_adjust_ms as u64)
        } else {
            head_unix_ms.saturating_add(member.sync_adjust_ms.unsigned_abs())
        };

        anchor = anchor.max(
            unadjusted_head.saturating_add(AIRPLAY_SPLICE_LEAD_MARGIN_MS),
        );
    }

    anchor
}

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
    fn warm_anchor_preserves_base_when_no_member_constraints_apply() {
        let members = [WarmGroupMemberConstraint {
            sync_adjust_ms: 0,
            warm_lead_ms: 0,
            flush_ack: GroupFlushAck::no_head_constraint(),
        }];

        assert_eq!(resolve_warm_group_anchor(10_000, 10_500, &members), 10_500);
    }

    #[test]
    fn negative_sync_adjust_expands_member_warm_lead() {
        let members = [WarmGroupMemberConstraint {
            sync_adjust_ms: -80,
            warm_lead_ms: 600,
            flush_ack: GroupFlushAck::no_head_constraint(),
        }];

        // 10_000 + (600 - min(0, -80)) + 150
        assert_eq!(resolve_warm_group_anchor(10_000, 10_500, &members), 10_830);
    }

    #[test]
    fn frozen_head_is_brought_back_to_common_group_timeline() {
        let members = [
            WarmGroupMemberConstraint {
                sync_adjust_ms: 50,
                warm_lead_ms: 0,
                flush_ack: GroupFlushAck::with_head(11_000),
            },
            WarmGroupMemberConstraint {
                sync_adjust_ms: -40,
                warm_lead_ms: 0,
                flush_ack: GroupFlushAck::with_head(10_900),
            },
        ];

        // member 1 => 11_000 - 50 + 150 = 11_100
        // member 2 => 10_900 - (-40) + 150 = 11_090
        assert_eq!(resolve_warm_group_anchor(10_000, 10_500, &members), 11_100);
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
