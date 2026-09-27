//! Transport-neutral shared PCM fan-out contract.
//!
//! Music Assistant owns one source stream per AirPlay session. Each source
//! chunk is handed to every currently-running member, all writes are awaited,
//! and failed members are removed before the next source chunk advances.
//!
//! This module normalizes only that orchestration shape. It does not own
//! WASAPI capture, resample audio, or join native/RAOP worker lifetimes.

use std::fmt;
use std::thread;
use std::time::Duration;

/// One persistent PCM source owned by the group session rather than by any
/// individual transport lane.
///
/// `Ok(None)` is temporary source starvation for the requested interval, not
/// end-of-stream. This matches the Phase A persistent WASAPI ring contract.
pub trait GroupPcmSource {
    fn read_shared_pcm(
        &self,
        want_bytes: usize,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String>;

    fn buffered_bytes(&self) -> usize;
}

/// One member sink participating in a shared PCM source session.
pub trait GroupPcmParticipant: Send {
    fn name(&self) -> &str;

    /// Consume the exact shared source chunk. Transport-specific format
    /// adaptation remains outside this common contract until Phase C.
    fn write_shared_pcm(&mut self, chunk: &[u8]) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPcmFailure {
    pub member: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupPcmPumpOutcome {
    Starved,
    Delivered {
        bytes: usize,
        failures: Vec<GroupPcmFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPcmCoordinatorCycle {
    pub outcome: GroupPcmPumpOutcome,
    pub removed_members: Vec<String>,
}

/// Execute exactly one MSA-style shared-source pump iteration:
///
/// 1. read the source once;
/// 2. treat temporary starvation as no advancement;
/// 3. deliver the exact same chunk to every active participant;
/// 4. wait for all writes and return all member failures together.
///
/// The caller owns membership removal and the outer loop. Keeping those
/// responsibilities outside this function avoids inventing mixed-session
/// lifecycle before native and RAOP orchestration are actually joined.
pub fn pump_shared_pcm_once(
    source: &dyn GroupPcmSource,
    want_bytes: usize,
    timeout: Duration,
    members: Vec<&mut dyn GroupPcmParticipant>,
) -> Result<GroupPcmPumpOutcome, String> {
    let Some(chunk) = source.read_shared_pcm(want_bytes, timeout)? else {
        return Ok(GroupPcmPumpOutcome::Starved);
    };

    let bytes = chunk.len();
    let failures = fanout_shared_pcm_chunk(members, &chunk);
    Ok(GroupPcmPumpOutcome::Delivered { bytes, failures })
}

/// Session-level owner for one shared PCM reader and its active transport sinks.
///
/// This is intentionally transport-neutral. It owns exactly one `GroupPcmSource`
/// value, so native and RAOP participants cannot independently advance the
/// source timeline through this API. Membership removal/lifecycle remains with
/// the higher-level AirPlay session until mixed transport orchestration is
/// wired and hardware-validated.
pub struct GroupPcmCoordinator<S: GroupPcmSource> {
    source: S,
    members: Vec<Box<dyn GroupPcmParticipant>>,
}

impl<S: GroupPcmSource> GroupPcmCoordinator<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            members: Vec::new(),
        }
    }

    pub fn add_member(
        &mut self,
        member: Box<dyn GroupPcmParticipant>,
    ) -> Result<(), String> {
        let name = member.name().to_owned();
        if self.members.iter().any(|existing| existing.name() == name) {
            return Err(format!(
                "duplicate shared PCM participant identity: {name}"
            ));
        }
        self.members.push(member);
        Ok(())
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    pub fn member_names(&self) -> Vec<String> {
        self.members
            .iter()
            .map(|member| member.name().to_owned())
            .collect()
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    pub fn pump_once(
        &mut self,
        want_bytes: usize,
        timeout: Duration,
    ) -> Result<GroupPcmCoordinatorCycle, String> {
        let members = self
            .members
            .iter_mut()
            .map(|member| member.as_mut() as &mut dyn GroupPcmParticipant)
            .collect::<Vec<_>>();

        let outcome = pump_shared_pcm_once(&self.source, want_bytes, timeout, members)?;
        let mut removed_members = Vec::new();

        if let GroupPcmPumpOutcome::Delivered { failures, .. } = &outcome {
            // Match MSA's write-cycle ordering: gather every member result for
            // this source chunk first, then remove failed players immediately
            // from the active set before the next source read. Transport/process
            // cleanup remains the higher-level session's responsibility.
            let failed_names = failures
                .iter()
                .filter(|failure| failure.member != "AirPlay group")
                .map(|failure| failure.member.as_str())
                .collect::<Vec<_>>();

            self.members.retain(|member| {
                if failed_names.iter().any(|failed| *failed == member.name()) {
                    removed_members.push(member.name().to_owned());
                    false
                } else {
                    true
                }
            });
        }

        Ok(GroupPcmCoordinatorCycle {
            outcome,
            removed_members,
        })
    }
}

impl fmt::Display for GroupPcmFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.member, self.error)
    }
}

/// Fan one source chunk to every member concurrently and wait for every write
/// to finish before returning. Failures retain member identity so the session
/// controller can remove those members before advancing the shared source.
///
/// Unlike START, a PCM fan-out does not fail-fast: Music Assistant gathers all
/// member results for the chunk, then removes every failed player.
pub fn fanout_shared_pcm_chunk(
    members: Vec<&mut dyn GroupPcmParticipant>,
    chunk: &[u8],
) -> Vec<GroupPcmFailure> {
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(members.len());

        for member in members {
            handles.push(scope.spawn(move || {
                let name = member.name().to_owned();
                member
                    .write_shared_pcm(chunk)
                    .map_err(|error| GroupPcmFailure {
                        member: name,
                        error,
                    })
            })).unwrap();
        }

        let mut failures = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(error),
                Err(_) => failures.push(GroupPcmFailure {
                    member: "AirPlay group".to_owned(),
                    error: "shared PCM writer panicked".to_owned(),
                }),
            }
        }
        failures
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeSource {
        reads: Arc<Mutex<usize>>,
        chunk: Option<Vec<u8>>,
    }

    impl GroupPcmSource for FakeSource {
        fn read_shared_pcm(
            &self,
            _want_bytes: usize,
            _timeout: Duration,
        ) -> Result<Option<Vec<u8>>, String> {
            *self.reads.lock().unwrap() += 1;
            Ok(self.chunk.clone())
        }

        fn buffered_bytes(&self) -> usize {
            self.chunk.as_ref().map(|chunk| chunk.len()).unwrap_or(0)
        }
    }

    struct FakeMember {
        name: &'static str,
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: bool,
    }

    impl GroupPcmParticipant for FakeMember {
        fn name(&self) -> &str {
            self.name
        }

        fn write_shared_pcm(&mut self, chunk: &[u8]) -> Result<(), String> {
            self.seen.lock().unwrap().push(chunk.to_vec());
            if self.fail {
                Err("write failed".to_owned())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn every_member_receives_the_exact_same_source_chunk() {
        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        let mut a = FakeMember {
            name: "native",
            seen: Arc::clone(&seen_a),
            fail: false,
        };
        let mut b = FakeMember {
            name: "raop",
            seen: Arc::clone(&seen_b),
            fail: false,
        };
        let chunk = vec![1u8, 2, 3, 4, 5, 6];

        let failures =
            fanout_shared_pcm_chunk(vec![&mut a, &mut b], &chunk);

        assert!(failures.is_empty());
        assert_eq!(*seen_a.lock().unwrap(), vec![chunk.clone()]);
        assert_eq!(*seen_b.lock().unwrap(), vec![chunk]);
    }

    #[test]
    fn collects_all_member_failures_without_hiding_successful_writes() {
        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        let seen_c = Arc::new(Mutex::new(Vec::new()));
        let mut a = FakeMember {
            name: "native-ok",
            seen: Arc::clone(&seen_a),
            fail: false,
        };
        let mut b = FakeMember {
            name: "raop-bad",
            seen: Arc::clone(&seen_b),
            fail: true,
        };
        let mut c = FakeMember {
            name: "native-bad",
            seen: Arc::clone(&seen_c),
            fail: true,
        };

        let failures = fanout_shared_pcm_chunk(
            vec![&mut a, &mut b, &mut c],
            &[9u8, 8, 7],
        );

        assert_eq!(failures.len(), 2);
        assert!(failures.iter().any(|f| f.member == "raop-bad"));
        assert!(failures.iter().any(|f| f.member == "native-bad"));
        assert_eq!(seen_a.lock().unwrap().len(), 1);
        assert_eq!(seen_b.lock().unwrap().len(), 1);
        assert_eq!(seen_c.lock().unwrap().len(), 1);
    }

    #[test]
    fn empty_group_is_a_noop_for_the_source_chunk() {
        let failures = fanout_shared_pcm_chunk(Vec::new(), &[1u8, 2, 3]);
        assert!(failures.is_empty());
    }

    #[test]
    fn pump_reads_source_once_then_fans_exact_chunk_to_all_members() {
        let reads = Arc::new(Mutex::new(0usize));
        let source = FakeSource {
            reads: Arc::clone(&reads),
            chunk: Some(vec![4u8, 5, 6, 7]),
        };
        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        let mut a = FakeMember {
            name: "native",
            seen: Arc::clone(&seen_a),
            fail: false,
        };
        let mut b = FakeMember {
            name: "raop",
            seen: Arc::clone(&seen_b),
            fail: false,
        };

        let outcome = pump_shared_pcm_once(
            &source,
            4,
            Duration::from_millis(0),
            vec![&mut a, &mut b],
        )
        .unwrap();

        assert_eq!(*reads.lock().unwrap(), 1);
        assert_eq!(
            outcome,
            GroupPcmCoordinatorCycle {
                outcome: GroupPcmPumpOutcome::Delivered {
                    bytes: 4,
                    failures: Vec::new(),
                },
                removed_members: Vec::new(),
            }
        );
        assert_eq!(*seen_a.lock().unwrap(), vec![vec![4u8, 5, 6, 7]]);
        assert_eq!(*seen_b.lock().unwrap(), vec![vec![4u8, 5, 6, 7]]);
    }

    #[test]
    fn pump_starvation_does_not_write_any_member() {
        let reads = Arc::new(Mutex::new(0usize));
        let source = FakeSource {
            reads: Arc::clone(&reads),
            chunk: None,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut member = FakeMember {
            name: "native",
            seen: Arc::clone(&seen),
            fail: false,
        };

        let outcome = pump_shared_pcm_once(
            &source,
            4,
            Duration::from_millis(0),
            vec![&mut member],
        )
        .unwrap();

        assert_eq!(*reads.lock().unwrap(), 1);
        assert_eq!(outcome, GroupPcmPumpOutcome::Starved);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn pump_returns_all_failures_after_one_source_read() {
        let reads = Arc::new(Mutex::new(0usize));
        let source = FakeSource {
            reads: Arc::clone(&reads),
            chunk: Some(vec![1u8, 2]),
        };
        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        let mut a = FakeMember {
            name: "native-bad",
            seen: Arc::clone(&seen_a),
            fail: true,
        };
        let mut b = FakeMember {
            name: "raop-bad",
            seen: Arc::clone(&seen_b),
            fail: true,
        };

        let outcome = pump_shared_pcm_once(
            &source,
            2,
            Duration::from_millis(0),
            vec![&mut a, &mut b],
        )
        .unwrap();

        assert_eq!(*reads.lock().unwrap(), 1);
        let GroupPcmPumpOutcome::Delivered { bytes, failures } = outcome else {
            panic!("expected delivered outcome");
        };
        assert_eq!(bytes, 2);
        assert_eq!(failures.len(), 2);
        assert!(failures.iter().any(|failure| failure.member == "native-bad"));
        assert!(failures.iter().any(|failure| failure.member == "raop-bad"));
    }

    #[test]
    fn coordinator_owns_one_source_reader_for_multiple_transport_sinks() {
        let reads = Arc::new(Mutex::new(0usize));
        let source = FakeSource {
            reads: Arc::clone(&reads),
            chunk: Some(vec![7u8, 7, 7, 7]),
        };
        let seen_native = Arc::new(Mutex::new(Vec::new()));
        let seen_raop = Arc::new(Mutex::new(Vec::new()));

        let mut coordinator = GroupPcmCoordinator::new(source);
        coordinator.add_member(Box::new(FakeMember {
            name: "native",
            seen: Arc::clone(&seen_native),
            fail: false,
        })).unwrap();
        coordinator.add_member(Box::new(FakeMember {
            name: "raop",
            seen: Arc::clone(&seen_raop),
            fail: false,
        })).unwrap();

        assert_eq!(coordinator.member_count(), 2);
        assert_eq!(
            coordinator.member_names(),
            vec!["native".to_owned(), "raop".to_owned()]
        );

        let outcome = coordinator
            .pump_once(4, Duration::from_millis(0))
            .unwrap();

        assert_eq!(*reads.lock().unwrap(), 1);
        assert_eq!(
            outcome,
            GroupPcmPumpOutcome::Delivered {
                bytes: 4,
                failures: Vec::new(),
            }
        );
        assert_eq!(*seen_native.lock().unwrap(), vec![vec![7u8, 7, 7, 7]]);
        assert_eq!(*seen_raop.lock().unwrap(), vec![vec![7u8, 7, 7, 7]]);
    }

    #[test]
    fn coordinator_starvation_keeps_all_sinks_idle() {
        let reads = Arc::new(Mutex::new(0usize));
        let source = FakeSource {
            reads: Arc::clone(&reads),
            chunk: None,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));

        let mut coordinator = GroupPcmCoordinator::new(source);
        coordinator.add_member(Box::new(FakeMember {
            name: "native",
            seen: Arc::clone(&seen),
            fail: false,
        })).unwrap();

        assert_eq!(
            coordinator
                .pump_once(4, Duration::from_millis(0))
                .unwrap(),
            GroupPcmCoordinatorCycle {
                outcome: GroupPcmPumpOutcome::Starved,
                removed_members: Vec::new(),
            }
        );
        assert_eq!(*reads.lock().unwrap(), 1);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn coordinator_prunes_failed_member_before_next_source_read() {
        let reads = Arc::new(Mutex::new(0usize));
        let source = FakeSource {
            reads: Arc::clone(&reads),
            chunk: Some(vec![3u8, 3]),
        };
        let seen_ok = Arc::new(Mutex::new(Vec::new()));
        let seen_bad = Arc::new(Mutex::new(Vec::new()));

        let mut coordinator = GroupPcmCoordinator::new(source);
        coordinator
            .add_member(Box::new(FakeMember {
                name: "native-ok",
                seen: Arc::clone(&seen_ok),
                fail: false,
            }))
            .unwrap();
        coordinator
            .add_member(Box::new(FakeMember {
                name: "raop-bad",
                seen: Arc::clone(&seen_bad),
                fail: true,
            }))
            .unwrap();

        let first = coordinator
            .pump_once(2, Duration::from_millis(0))
            .unwrap();
        assert_eq!(first.removed_members, vec!["raop-bad".to_owned()]);
        assert_eq!(coordinator.member_names(), vec!["native-ok".to_owned()]);

        let second = coordinator
            .pump_once(2, Duration::from_millis(0))
            .unwrap();
        assert!(second.removed_members.is_empty());
        assert_eq!(*reads.lock().unwrap(), 2);
        assert_eq!(seen_ok.lock().unwrap().len(), 2);
        assert_eq!(seen_bad.lock().unwrap().len(), 1);
    }

    #[test]
    fn coordinator_rejects_duplicate_member_identity() {
        let source = FakeSource {
            reads: Arc::new(Mutex::new(0usize)),
            chunk: None,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut coordinator = GroupPcmCoordinator::new(source);

        coordinator
            .add_member(Box::new(FakeMember {
                name: "same-device",
                seen: Arc::clone(&seen),
                fail: false,
            }))
            .unwrap();

        let error = coordinator
            .add_member(Box::new(FakeMember {
                name: "same-device",
                seen,
                fail: false,
            }))
            .unwrap_err();

        assert!(error.contains("duplicate shared PCM participant identity"));
        assert_eq!(coordinator.member_count(), 1);
    }

}
