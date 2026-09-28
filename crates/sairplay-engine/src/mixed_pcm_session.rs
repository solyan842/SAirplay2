//! Transport-neutral mixed PCM session shell.
//!
//! This layer deliberately owns only the common coordinator lifecycle. It does
//! not own WASAPI, native AirPlay 2 transport, or RAOP helpers. Those adapters
//! are attached later so Stable3 native-only playback remains untouched.

use crate::{
    GroupPcmCoordinator, GroupPcmCoordinatorCycle, GroupPcmParticipant,
    GroupPcmSource,
};
use std::time::Duration;

pub struct MixedPcmSession<S: GroupPcmSource> {
    coordinator: GroupPcmCoordinator<S>,
}

impl<S: GroupPcmSource> MixedPcmSession<S> {
    pub fn new(source: S) -> Self {
        Self {
            coordinator: GroupPcmCoordinator::new(source),
        }
    }

    pub fn add_member(
        &mut self,
        member: Box<dyn GroupPcmParticipant>,
    ) -> Result<(), String> {
        self.coordinator.add_member(member)
    }

    pub fn remove_member(&mut self, name: &str) -> bool {
        self.coordinator.remove_member(name)
    }

    pub fn member_count(&self) -> usize {
        self.coordinator.member_count()
    }

    pub fn member_names(&self) -> Vec<String> {
        self.coordinator.member_names()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.coordinator.source().buffered_bytes()
    }

    pub fn pump_once(
        &mut self,
        want_bytes: usize,
        timeout: Duration,
    ) -> Result<GroupPcmCoordinatorCycle, String> {
        self.coordinator.pump_once(want_bytes, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GroupPcmCoordinatorCycle, GroupPcmPumpOutcome};
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
            self.chunk.as_ref().map(Vec::len).unwrap_or(0)
        }
    }

    struct FakeSink {
        name: &'static str,
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl GroupPcmParticipant for FakeSink {
        fn name(&self) -> &str {
            self.name
        }

        fn write_shared_pcm(&mut self, chunk: &[u8]) -> Result<(), String> {
            self.seen.lock().unwrap().push(chunk.to_vec());
            Ok(())
        }
    }

    #[test]
    fn mixed_shell_keeps_one_source_and_multiple_members() {
        let reads = Arc::new(Mutex::new(0usize));
        let seen_native = Arc::new(Mutex::new(Vec::new()));
        let seen_raop = Arc::new(Mutex::new(Vec::new()));

        let mut session = MixedPcmSession::new(FakeSource {
            reads: Arc::clone(&reads),
            chunk: Some(vec![1, 2, 3, 4]),
        });
        session.add_member(Box::new(FakeSink {
            name: "native",
            seen: Arc::clone(&seen_native),
        })).unwrap();
        session.add_member(Box::new(FakeSink {
            name: "raop",
            seen: Arc::clone(&seen_raop),
        })).unwrap();

        assert_eq!(session.member_count(), 2);
        assert_eq!(session.buffered_bytes(), 4);
        assert_eq!(
            session.pump_once(4, Duration::from_millis(0)).unwrap(),
            GroupPcmCoordinatorCycle {
                outcome: GroupPcmPumpOutcome::Delivered {
                    bytes: 4,
                    failures: Vec::new(),
                },
                removed_members: Vec::new(),
            }
        );
        assert_eq!(*reads.lock().unwrap(), 1);
        assert_eq!(seen_native.lock().unwrap().len(), 1);
        assert_eq!(seen_raop.lock().unwrap().len(), 1);
    }

    #[test]
    fn mixed_shell_remove_happens_before_next_read() {
        let reads = Arc::new(Mutex::new(0usize));
        let seen = Arc::new(Mutex::new(Vec::new()));

        let mut session = MixedPcmSession::new(FakeSource {
            reads: Arc::clone(&reads),
            chunk: Some(vec![9, 9]),
        });
        session.add_member(Box::new(FakeSink {
            name: "native",
            seen: Arc::clone(&seen),
        })).unwrap();
        session.add_member(Box::new(FakeSink {
            name: "raop",
            seen: Arc::new(Mutex::new(Vec::new())),
        })).unwrap();

        assert!(session.remove_member("raop"));
        assert_eq!(session.member_names(), vec!["native".to_owned()]);
        session.pump_once(2, Duration::from_millis(0)).unwrap();
        assert_eq!(*reads.lock().unwrap(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
