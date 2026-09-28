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
            }));
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
}
