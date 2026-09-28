//! MSA-clone session lifecycle adapter.
//!
//! Mirrors pinned Music Assistant `ap2_session.c` ordering:
//! quiesce transport -> commit/flush/stop -> update session core -> resume.
//! The transport remains protocol-specific behind this trait.

use crate::{MsaSessionCore, MsaSessionState};

pub trait MsaSessionTransport {
    fn quiesce(&mut self) -> Result<(), String>;
    fn commit_start(
        &mut self,
        requested_start_unix_ms: u64,
    ) -> Result<u64, String>;
    fn flush(&mut self) -> Result<Option<u64>, String>;
    fn stop(&mut self) -> Result<(), String>;
    fn resume(&mut self) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaStartAck {
    pub requested_unix_ms: u64,
    pub committed_unix_ms: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaFlushAck {
    pub warm_head_unix_ms: Option<u64>,
}

pub struct MsaSession<T: MsaSessionTransport> {
    core: MsaSessionCore,
    transport: T,
}

impl<T: MsaSessionTransport> MsaSession<T> {
    pub fn new(core: MsaSessionCore, transport: T) -> Self {
        Self { core, transport }
    }

    pub fn core(&self) -> &MsaSessionCore {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut MsaSessionCore {
        &mut self.core
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn start(&mut self, requested_start_unix_ms: u64) -> Result<MsaStartAck, String> {
        if self.core.state() == MsaSessionState::Ended {
            return Err("cannot START an ended MSA session".into());
        }

        self.transport.quiesce()?;
        let committed = match self.transport.commit_start(requested_start_unix_ms) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.transport.resume();
                return Err(error);
            }
        };

        let epoch = match self.core.start_committed() {
            Ok(epoch) => epoch,
            Err(error) => {
                let _ = self.transport.resume();
                return Err(error);
            }
        };

        self.transport.resume()?;
        Ok(MsaStartAck {
            requested_unix_ms: requested_start_unix_ms,
            committed_unix_ms: committed,
            epoch,
        })
    }

    pub fn flush(&mut self) -> Result<MsaFlushAck, String> {
        if self.core.state() == MsaSessionState::Ended {
            return Err("cannot FLUSH an ended MSA session".into());
        }

        self.transport.quiesce()?;
        let head = match self.transport.flush() {
            Ok(head) => head,
            Err(error) => {
                let _ = self.transport.resume();
                return Err(error);
            }
        };

        if let Err(error) = self.core.flush_committed() {
            let _ = self.transport.resume();
            return Err(error);
        }

        self.transport.resume()?;
        Ok(MsaFlushAck {
            warm_head_unix_ms: head,
        })
    }

    pub fn standby(&mut self) -> Result<(), String> {
        if self.core.state() == MsaSessionState::Ended {
            return Err("cannot STANDBY an ended MSA session".into());
        }

        self.transport.quiesce()?;
        if let Err(error) = self.transport.stop() {
            let _ = self.transport.resume();
            return Err(error);
        }
        if let Err(error) = self.core.standby_committed() {
            let _ = self.transport.resume();
            return Err(error);
        }
        self.transport.resume()
    }

    pub fn end(&mut self) {
        self.core.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        calls: Vec<&'static str>,
        start_ack: u64,
        flush_head: Option<u64>,
        fail_commit: bool,
        fail_flush: bool,
        fail_stop: bool,
    }

    impl MsaSessionTransport for FakeTransport {
        fn quiesce(&mut self) -> Result<(), String> {
            self.calls.push("quiesce");
            Ok(())
        }
        fn commit_start(&mut self, requested: u64) -> Result<u64, String> {
            self.calls.push("commit");
            if self.fail_commit {
                Err("commit failed".into())
            } else {
                Ok(if self.start_ack == 0 { requested } else { self.start_ack })
            }
        }
        fn flush(&mut self) -> Result<Option<u64>, String> {
            self.calls.push("flush");
            if self.fail_flush {
                Err("flush failed".into())
            } else {
                Ok(self.flush_head)
            }
        }
        fn stop(&mut self) -> Result<(), String> {
            self.calls.push("stop");
            if self.fail_stop {
                Err("stop failed".into())
            } else {
                Ok(())
            }
        }
        fn resume(&mut self) -> Result<(), String> {
            self.calls.push("resume");
            Ok(())
        }
    }

    fn session() -> MsaSession<FakeTransport> {
        MsaSession::new(
            MsaSessionCore::new(176_400, 1_408).unwrap(),
            FakeTransport::default(),
        )
    }

    #[test]
    fn start_order_matches_msa_session() {
        let mut s = session();
        s.core_mut().push_input(&vec![1u8; 1_408]);
        let ack = s.start(1_700_000_000_000).unwrap();
        assert_eq!(ack.epoch, 1);
        assert_eq!(
            s.transport().calls,
            vec!["quiesce", "commit", "resume"]
        );
        assert_eq!(s.core().state(), MsaSessionState::Playing);
    }

    #[test]
    fn failed_start_resumes_without_advancing_epoch() {
        let mut s = session();
        s.transport_mut().fail_commit = true;
        assert!(s.start(123).is_err());
        assert_eq!(s.core().epoch(), 0);
        assert_eq!(s.core().state(), MsaSessionState::Idle);
        assert_eq!(
            s.transport().calls,
            vec!["quiesce", "commit", "resume"]
        );
    }

    #[test]
    fn flush_order_discards_old_source_then_returns_idle() {
        let mut s = session();
        s.core_mut().push_input(&vec![7u8; 2_816]);
        s.start(1000).unwrap();
        s.transport_mut().calls.clear();
        s.transport_mut().flush_head = Some(2000);

        let ack = s.flush().unwrap();
        assert_eq!(ack.warm_head_unix_ms, Some(2000));
        assert_eq!(
            s.transport().calls,
            vec!["quiesce", "flush", "resume"]
        );
        assert_eq!(s.core().state(), MsaSessionState::Idle);
        assert_eq!(s.core().buffered_bytes(), 0);
        assert!(!s.core().audio_present());
    }

    #[test]
    fn failed_flush_keeps_source_state_and_resumes() {
        let mut s = session();
        s.core_mut().push_input(&vec![2u8; 1_408]);
        s.start(1000).unwrap();
        s.transport_mut().calls.clear();
        s.transport_mut().fail_flush = true;
        let before = s.core().buffered_bytes();

        assert!(s.flush().is_err());
        assert_eq!(s.core().state(), MsaSessionState::Playing);
        assert_eq!(s.core().buffered_bytes(), before);
        assert_eq!(
            s.transport().calls,
            vec!["quiesce", "flush", "resume"]
        );
    }

    #[test]
    fn standby_order_matches_msa_session() {
        let mut s = session();
        s.start(1000).unwrap();
        s.transport_mut().calls.clear();

        s.standby().unwrap();
        assert_eq!(
            s.transport().calls,
            vec!["quiesce", "stop", "resume"]
        );
        assert_eq!(s.core().state(), MsaSessionState::Standby);
    }
}
