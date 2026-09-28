//! Music Assistant server-style group START orchestration.
//!
//! Source: pinned `music_assistant/providers/airplay/stream_session.py`.
//! This is the group/session layer above the protocol-specific AP2/RAOP
//! transport adapters. It plans one audible anchor, commands all members,
//! consumes each member's TRUE acknowledged instant and retries the whole group
//! when one member corrected forward.

use std::thread;

pub const MSA_COLD_GROUP_START_LEAD_MS: u64 = 2500;
pub const MSA_GROUP_START_LEAD_MS: u64 = 500;
pub const MSA_CLOCK_READY_LEAD_MS: u64 = 500;
pub const MSA_SPLICE_LEAD_MARGIN_MS: u64 = 150;
pub const MSA_START_TOLERANCE_MS: u64 = 2;
pub const MSA_START_MAX_ROUNDS: usize = 4;

pub trait MsaGroupStartMember: Send {
    fn name(&self) -> &str;
    fn sync_adjust_ms(&self) -> i64;

    /// Command START and return the transport's true acknowledged audible
    /// instant, including the member's sync-adjusted command.
    fn start_at(
        &mut self,
        commanded_unix_ms: u64,
        position_ms: u64,
    ) -> Result<u64, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaGroupStartFailure {
    pub member: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsaGroupStartResult {
    pub anchor_unix_ms: u64,
    pub rounds: usize,
    pub last_member_acks_unix_ms: Vec<(String, u64)>,
    pub converged: bool,
}

/// MSA initial group anchor:
/// max(now + cold-group lead, latest projected clock-ready + readiness lead).
pub fn msa_initial_group_anchor_unix_ms(
    now_unix_ms: u64,
    latest_clock_ready_unix_ms: u64,
) -> u64 {
    let mut anchor = now_unix_ms.saturating_add(MSA_COLD_GROUP_START_LEAD_MS);
    if latest_clock_ready_unix_ms != 0 {
        anchor = anchor.max(
            latest_clock_ready_unix_ms.saturating_add(MSA_CLOCK_READY_LEAD_MS),
        );
    }
    anchor
}

/// Execute one MSA group START round concurrently.
fn start_round(
    members: Vec<&mut dyn MsaGroupStartMember>,
    target_unix_ms: u64,
    position_ms: u64,
) -> Result<Vec<(String, i64, u64)>, Vec<MsaGroupStartFailure>> {
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(members.len());
        for member in members {
            handles.push(scope.spawn(move || {
                let name = member.name().to_owned();
                let adjust = member.sync_adjust_ms();
                let commanded = if adjust >= 0 {
                    target_unix_ms.saturating_add(adjust as u64)
                } else {
                    target_unix_ms.saturating_sub(adjust.unsigned_abs())
                };
                member
                    .start_at(commanded, position_ms)
                    .map(|ack| (name, adjust, ack))
                    .map_err(|error| MsaGroupStartFailure {
                        member: name,
                        error,
                    })
            }));
        }

        let mut ok = Vec::new();
        let mut failures = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(value)) => ok.push(value),
                Ok(Err(error)) => failures.push(error),
                Err(_) => failures.push(MsaGroupStartFailure {
                    member: "AirPlay group".to_owned(),
                    error: "group START member panicked".to_owned(),
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

/// MSA convergence loop from `AirPlayStreamSession._start_members`.
///
/// Each member is commanded at target + sync_adjust. Its ACK is normalized back
/// by subtracting that same adjustment. If any normalized ACK moved more than
/// 2 ms forward, every member is re-STARTed at the largest corrected instant
/// plus 150 ms. A solo member adopts its corrected ACK without a second START.
/// At most four rounds are attempted.
pub fn msa_start_group(
    members: &mut [Box<dyn MsaGroupStartMember>],
    position_ms: u64,
    start_unix_ms: u64,
) -> Result<MsaGroupStartResult, Vec<MsaGroupStartFailure>> {
    if members.is_empty() {
        return Ok(MsaGroupStartResult {
            anchor_unix_ms: start_unix_ms,
            rounds: 0,
            last_member_acks_unix_ms: Vec::new(),
            converged: true,
        });
    }

    let mut target_ms = start_unix_ms;
    let mut corrected_ms = start_unix_ms;
    let mut last_acks = Vec::new();

    for round in 1..=MSA_START_MAX_ROUNDS {
        let refs = members
            .iter_mut()
            .map(|member| member.as_mut() as &mut dyn MsaGroupStartMember)
            .collect::<Vec<_>>();

        let results = start_round(refs, target_ms, position_ms)?;

        corrected_ms = target_ms;
        last_acks.clear();
        for (name, adjust, ack) in results {
            let normalized = if adjust >= 0 {
                ack.saturating_sub(adjust as u64)
            } else {
                ack.saturating_add(adjust.unsigned_abs())
            };
            corrected_ms = corrected_ms.max(normalized);
            last_acks.push((name, normalized));
        }

        if corrected_ms <= target_ms.saturating_add(MSA_START_TOLERANCE_MS) {
            return Ok(MsaGroupStartResult {
                anchor_unix_ms: target_ms,
                rounds: round,
                last_member_acks_unix_ms: last_acks,
                converged: true,
            });
        }

        if members.len() == 1 {
            return Ok(MsaGroupStartResult {
                anchor_unix_ms: corrected_ms,
                rounds: round,
                last_member_acks_unix_ms: last_acks,
                converged: true,
            });
        }

        if round < MSA_START_MAX_ROUNDS {
            target_ms = corrected_ms.saturating_add(MSA_SPLICE_LEAD_MARGIN_MS);
        }
    }

    // Matches MSA: after the final unsuccessful round, record the instant the
    // members actually reported, not a retry target that was never commanded.
    Ok(MsaGroupStartResult {
        anchor_unix_ms: corrected_ms,
        rounds: MSA_START_MAX_ROUNDS,
        last_member_acks_unix_ms: last_acks,
        converged: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeMember {
        name: &'static str,
        adjust: i64,
        acks: Arc<Mutex<Vec<u64>>>,
        commanded: Arc<Mutex<Vec<u64>>>,
    }

    impl MsaGroupStartMember for FakeMember {
        fn name(&self) -> &str { self.name }
        fn sync_adjust_ms(&self) -> i64 { self.adjust }

        fn start_at(
            &mut self,
            commanded_unix_ms: u64,
            _position_ms: u64,
        ) -> Result<u64, String> {
            self.commanded.lock().unwrap().push(commanded_unix_ms);
            let mut acks = self.acks.lock().unwrap();
            if acks.is_empty() {
                Ok(commanded_unix_ms)
            } else {
                Ok(acks.remove(0))
            }
        }
    }

    fn member(
        name: &'static str,
        adjust: i64,
        acks: Vec<u64>,
    ) -> (Box<dyn MsaGroupStartMember>, Arc<Mutex<Vec<u64>>>) {
        let commanded = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(FakeMember {
                name,
                adjust,
                acks: Arc::new(Mutex::new(acks)),
                commanded: Arc::clone(&commanded),
            }),
            commanded,
        )
    }

    #[test]
    fn cold_anchor_waits_for_latest_clock_projection() {
        assert_eq!(msa_initial_group_anchor_unix_ms(10_000, 0), 12_500);
        assert_eq!(
            msa_initial_group_anchor_unix_ms(10_000, 13_000),
            13_500
        );
    }

    #[test]
    fn exact_group_start_converges_in_one_round() {
        let (a, ca) = member("a", 0, vec![12_500]);
        let (b, cb) = member("b", 0, vec![12_500]);
        let mut members = vec![a, b];

        let result = msa_start_group(&mut members, 0, 12_500).unwrap();

        assert!(result.converged);
        assert_eq!(result.anchor_unix_ms, 12_500);
        assert_eq!(result.rounds, 1);
        assert_eq!(*ca.lock().unwrap(), vec![12_500]);
        assert_eq!(*cb.lock().unwrap(), vec![12_500]);
    }

    #[test]
    fn corrected_member_reanchors_whole_group_with_margin() {
        let (a, ca) = member("a", 0, vec![12_500, 12_850]);
        let (b, cb) = member("b", 0, vec![12_700, 12_850]);
        let mut members = vec![a, b];

        let result = msa_start_group(&mut members, 0, 12_500).unwrap();

        assert!(result.converged);
        assert_eq!(result.anchor_unix_ms, 12_850);
        assert_eq!(result.rounds, 2);
        assert_eq!(*ca.lock().unwrap(), vec![12_500, 12_850]);
        assert_eq!(*cb.lock().unwrap(), vec![12_500, 12_850]);
    }

    #[test]
    fn solo_adopts_corrected_ack_without_second_start() {
        let (a, ca) = member("solo", 0, vec![12_700]);
        let mut members = vec![a];

        let result = msa_start_group(&mut members, 0, 12_500).unwrap();

        assert!(result.converged);
        assert_eq!(result.anchor_unix_ms, 12_700);
        assert_eq!(result.rounds, 1);
        assert_eq!(*ca.lock().unwrap(), vec![12_500]);
    }

    #[test]
    fn sync_adjust_is_removed_from_convergence_comparison() {
        let (a, ca) = member("a", 25, vec![12_525]);
        let (b, cb) = member("b", -10, vec![12_490]);
        let mut members = vec![a, b];

        let result = msa_start_group(&mut members, 0, 12_500).unwrap();

        assert!(result.converged);
        assert_eq!(result.anchor_unix_ms, 12_500);
        assert_eq!(*ca.lock().unwrap(), vec![12_525]);
        assert_eq!(*cb.lock().unwrap(), vec![12_490]);
    }

    #[test]
    fn fourth_failed_round_records_last_truth_not_unissued_retry() {
        let (a, _) = member("a", 0, vec![12_600, 12_900, 13_200, 13_500]);
        let (b, _) = member("b", 0, vec![12_650, 12_950, 13_250, 13_550]);
        let mut members = vec![a, b];

        let result = msa_start_group(&mut members, 0, 12_500).unwrap();

        assert!(!result.converged);
        assert_eq!(result.rounds, 4);
        assert_eq!(result.anchor_unix_ms, 13_550);
    }
}
