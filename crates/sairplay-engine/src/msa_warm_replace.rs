//! Music Assistant server-style warm replacement orchestration.
//!
//! Mirrors pinned `AirPlayStreamSession.replace()` + `_anchor_start_unix_ms(warm=True)`:
//! old source is stopped by the owner before this step; every member is flushed
//! in-place; all flush acknowledgements are collected before the new source is
//! considered; once new audio is present, one warm anchor is computed and the
//! existing MSA group START convergence loop re-anchors the session.

use crate::{
    msa_start_group, MsaGroupStartFailure, MsaGroupStartMember, MsaGroupStartResult,
    MSA_GROUP_START_LEAD_MS, MSA_SPLICE_LEAD_MARGIN_MS,
};
use std::thread;

pub trait MsaWarmReplaceMember: MsaGroupStartMember {
    /// Flush the member's live transport in place and return the frozen audible
    /// head, when the transport exposes one. RAOP normally returns None.
    fn flush_for_replace(&mut self) -> Result<Option<u64>, String>;

    /// Minimum warm lead the transport requires before it can splice new audio.
    fn warm_lead_ms(&self) -> u64;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaWarmFlushFailure {
    pub member: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaWarmMemberSnapshot {
    pub name: String,
    pub sync_adjust_ms: i64,
    pub warm_lead_ms: u64,
    pub flushed_head_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaWarmReplaceResult {
    pub warm_anchor_unix_ms: u64,
    pub start: MsaGroupStartResult,
    pub members: Vec<MsaWarmMemberSnapshot>,
}

pub fn msa_flush_group_for_replace(
    members: &mut [Box<dyn MsaWarmReplaceMember>],
) -> Result<Vec<MsaWarmMemberSnapshot>, Vec<MsaWarmFlushFailure>> {
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(members.len());
        for member in members.iter_mut() {
            handles.push(scope.spawn(move || {
                let name = member.name().to_owned();
                let adjust = member.sync_adjust_ms();
                let warm_lead = member.warm_lead_ms();
                match member.flush_for_replace() {
                    Ok(head) => Ok(MsaWarmMemberSnapshot {
                        name,
                        sync_adjust_ms: adjust,
                        warm_lead_ms: warm_lead,
                        flushed_head_unix_ms: head,
                    }),
                    Err(error) => Err(MsaWarmFlushFailure {
                        member: name,
                        error,
                    }),
                }
            }));
        }

        let mut ok = Vec::new();
        let mut failures = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(value)) => ok.push(value),
                Ok(Err(error)) => failures.push(error),
                Err(_) => failures.push(MsaWarmFlushFailure {
                    member: "AirPlay warm group".to_owned(),
                    error: "warm FLUSH member panicked".to_owned(),
                }),
            }
        }

        if failures.is_empty() {
            Ok(ok)
        } else {
            Err(failures)
        }
    })
}

/// Exact MSA warm-anchor formula:
/// - base: now + group warm lead (500 ms);
/// - for each warm-lead member: now + warm_lead - min(0, sync_adjust) + 150 ms;
/// - for each frozen flush head: flushed_head - sync_adjust + 150 ms.
///
/// The sync-adjust algebra mirrors Music Assistant's commanded instant:
/// member command = group anchor + sync_adjust.
pub fn msa_warm_anchor_unix_ms(
    now_unix_ms: u64,
    members: &[MsaWarmMemberSnapshot],
) -> u64 {
    let mut anchor = now_unix_ms.saturating_add(MSA_GROUP_START_LEAD_MS);

    for member in members {
        if member.warm_lead_ms > 0 {
            let negative_adjust_compensation = if member.sync_adjust_ms < 0 {
                member.sync_adjust_ms.unsigned_abs()
            } else {
                0
            };
            let requirement = member
                .warm_lead_ms
                .saturating_add(negative_adjust_compensation);
            anchor = anchor.max(
                now_unix_ms
                    .saturating_add(requirement)
                    .saturating_add(MSA_SPLICE_LEAD_MARGIN_MS),
            );
        }

        if let Some(head) = member.flushed_head_unix_ms {
            let adjusted_requirement = if member.sync_adjust_ms >= 0 {
                head.saturating_sub(member.sync_adjust_ms as u64)
            } else {
                head.saturating_add(member.sync_adjust_ms.unsigned_abs())
            };
            anchor = anchor.max(
                adjusted_requirement.saturating_add(MSA_SPLICE_LEAD_MARGIN_MS),
            );
        }
    }

    anchor
}

/// Execute the re-anchor half after the owner has started feeding the new
/// source and confirmed audio-present for every member.
pub fn msa_start_warm_replacement(
    members: &mut [Box<dyn MsaWarmReplaceMember>],
    position_ms: u64,
    now_unix_ms: u64,
    snapshots: Vec<MsaWarmMemberSnapshot>,
) -> Result<MsaWarmReplaceResult, Vec<MsaGroupStartFailure>> {
    let anchor = msa_warm_anchor_unix_ms(now_unix_ms, &snapshots);

    // Trait upcast through a temporary vector of thin forwarding refs is not
    // yet stable for boxed trait objects, so use an adapter that forwards the
    // START-facing portion only.
    struct StartView<'a> {
        inner: &'a mut dyn MsaWarmReplaceMember,
    }

    impl MsaGroupStartMember for StartView<'_> {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn sync_adjust_ms(&self) -> i64 {
            self.inner.sync_adjust_ms()
        }
        fn start_at(
            &mut self,
            commanded_unix_ms: u64,
            position_ms: u64,
        ) -> Result<u64, String> {
            self.inner.start_at(commanded_unix_ms, position_ms)
        }
    }

    let mut start_members: Vec<Box<dyn MsaGroupStartMember + '_>> = members
        .iter_mut()
        .map(|member| {
            Box::new(StartView {
                inner: member.as_mut(),
            }) as Box<dyn MsaGroupStartMember>
        })
        .collect();

    let start = msa_start_group(&mut start_members, position_ms, anchor)?;
    Ok(MsaWarmReplaceResult {
        warm_anchor_unix_ms: anchor,
        start,
        members: snapshots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeWarmMember {
        name: &'static str,
        adjust: i64,
        warm_lead: u64,
        flush_head: Option<u64>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl MsaGroupStartMember for FakeWarmMember {
        fn name(&self) -> &str { self.name }
        fn sync_adjust_ms(&self) -> i64 { self.adjust }
        fn start_at(&mut self, commanded_unix_ms: u64, _position_ms: u64)
            -> Result<u64, String>
        {
            self.calls.lock().unwrap().push("start");
            Ok(commanded_unix_ms)
        }
    }

    impl MsaWarmReplaceMember for FakeWarmMember {
        fn flush_for_replace(&mut self) -> Result<Option<u64>, String> {
            self.calls.lock().unwrap().push("flush");
            Ok(self.flush_head)
        }

        fn warm_lead_ms(&self) -> u64 {
            self.warm_lead
        }
    }

    fn member(
        name: &'static str,
        adjust: i64,
        warm_lead: u64,
        flush_head: Option<u64>,
    ) -> (Box<dyn MsaWarmReplaceMember>, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(FakeWarmMember {
                name,
                adjust,
                warm_lead,
                flush_head,
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }

    #[test]
    fn flushes_every_member_before_replacement_start_phase() {
        let (a, ca) = member("a", 0, 600, Some(10_900));
        let (b, cb) = member("b", 0, 0, None);
        let mut members = vec![a, b];

        let snapshots = msa_flush_group_for_replace(&mut members).unwrap();

        assert_eq!(snapshots.len(), 2);
        assert_eq!(*ca.lock().unwrap(), vec!["flush"]);
        assert_eq!(*cb.lock().unwrap(), vec!["flush"]);
    }

    #[test]
    fn warm_anchor_honors_base_lead_warm_lead_and_frozen_head() {
        let members = vec![
            MsaWarmMemberSnapshot {
                name: "a".into(),
                sync_adjust_ms: 0,
                warm_lead_ms: 800,
                flushed_head_unix_ms: Some(10_900),
            },
            MsaWarmMemberSnapshot {
                name: "b".into(),
                sync_adjust_ms: -50,
                warm_lead_ms: 600,
                flushed_head_unix_ms: Some(10_700),
            },
        ];

        // base 10_500
        // a warm lead => 10_950
        // a frozen head => 11_050 (winner)
        // b warm lead with -50 adjust => 10_800
        // b frozen head corrected for -50 => 10_900
        assert_eq!(msa_warm_anchor_unix_ms(10_000, &members), 11_050);
    }

    #[test]
    fn positive_sync_adjust_reduces_required_group_anchor_for_frozen_head() {
        let members = vec![MsaWarmMemberSnapshot {
            name: "a".into(),
            sync_adjust_ms: 40,
            warm_lead_ms: 0,
            flushed_head_unix_ms: Some(10_900),
        }];

        // group anchor + 40 is the member's command; therefore 10_860 + 40
        // clears the 10_900 frozen head, then +150 margin => 11_010.
        assert_eq!(msa_warm_anchor_unix_ms(10_000, &members), 11_010);
    }

    #[test]
    fn warm_start_uses_existing_group_convergence() {
        let (a, ca) = member("a", 0, 0, Some(10_500));
        let (b, cb) = member("b", 0, 0, Some(10_500));
        let mut members = vec![a, b];

        let snapshots = msa_flush_group_for_replace(&mut members).unwrap();
        let result = msa_start_warm_replacement(
            &mut members,
            25_000,
            10_000,
            snapshots,
        ).unwrap();

        assert!(result.start.converged);
        assert_eq!(result.warm_anchor_unix_ms, 10_650);
        assert_eq!(result.start.anchor_unix_ms, 10_650);
        assert_eq!(*ca.lock().unwrap(), vec!["flush", "start"]);
        assert_eq!(*cb.lock().unwrap(), vec!["flush", "start"]);
    }
}
