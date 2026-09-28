//! Native AirPlay 2 transport adapter for the SAirplay 2.0 MSA clone.
//!
//! This mirrors the transport-facing contract used by pinned Music Assistant
//! `cliairplay.c` + `ap2_client.c`:
//! - START delegates feasibility / receiver-clock correction to the AP2 client
//!   and returns the client's acknowledged audible instant;
//! - FLUSH discards the receiver buffer in place and exposes the frozen warm
//!   delivery head while sends are still quiesced;
//! - STANDBY parks content without tearing down the connected session.
//!
//! This module intentionally does not implement RTSP/PTP itself. The concrete
//! AP2 client backend is attached later from source-verified primitives.

use crate::MsaSessionTransport;

pub trait MsaAp2Client {
    /// Equivalent of `ap2cl_start(..., &at_unix_ms)`.
    fn start_at(&mut self, requested_start_unix_ms: u64) -> Result<u64, String>;

    /// Equivalent of `ap2cl_flush()`.
    fn flush_in_place(&mut self) -> Result<(), String>;

    /// Equivalent of `ap2cl_splice_head_unix_ms()`.
    fn warm_head_unix_ms(&self) -> Option<u64>;

    /// Equivalent of `ap2cl_standby()`: park, do not disconnect.
    fn standby(&mut self) -> Result<(), String>;
}

/// The session adapter already brackets every lifecycle command with
/// quiesce/resume. This gate records that command/audio mutual exclusion
/// explicitly so a concrete send loop cannot silently bypass it later.
pub struct MsaAp2Transport<C: MsaAp2Client> {
    client: C,
    quiesced: bool,
}

impl<C: MsaAp2Client> MsaAp2Transport<C> {
    pub fn new(client: C) -> Self {
        Self {
            client,
            quiesced: false,
        }
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }

    pub fn is_quiesced(&self) -> bool {
        self.quiesced
    }

    fn require_quiesced(&self, operation: &str) -> Result<(), String> {
        if self.quiesced {
            Ok(())
        } else {
            Err(format!(
                "MSA AP2 transport {operation} requires quiesced send path"
            ))
        }
    }
}

impl<C: MsaAp2Client> MsaSessionTransport for MsaAp2Transport<C> {
    fn quiesce(&mut self) -> Result<(), String> {
        if self.quiesced {
            return Err("MSA AP2 transport already quiesced".into());
        }
        self.quiesced = true;
        Ok(())
    }

    fn commit_start(
        &mut self,
        requested_start_unix_ms: u64,
    ) -> Result<u64, String> {
        self.require_quiesced("START")?;
        self.client.start_at(requested_start_unix_ms)
    }

    fn flush(&mut self) -> Result<Option<u64>, String> {
        self.require_quiesced("FLUSH")?;
        self.client.flush_in_place()?;
        // MSA captures the warm head while sends are still quiesced, before
        // the session adapter resumes the live splice line.
        Ok(self.client.warm_head_unix_ms())
    }

    fn stop(&mut self) -> Result<(), String> {
        self.require_quiesced("STANDBY")?;
        self.client.standby()
    }

    fn resume(&mut self) -> Result<(), String> {
        if !self.quiesced {
            return Err("MSA AP2 transport resume without quiesce".into());
        }
        self.quiesced = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MsaSession, MsaSessionCore, MsaSessionState};

    #[derive(Default)]
    struct FakeAp2Client {
        calls: Vec<&'static str>,
        corrected_start: Option<u64>,
        warm_head: Option<u64>,
        fail_start: bool,
        fail_flush: bool,
        fail_standby: bool,
    }

    impl MsaAp2Client for FakeAp2Client {
        fn start_at(&mut self, requested: u64) -> Result<u64, String> {
            self.calls.push("ap2_start");
            if self.fail_start {
                Err("ap2 start failed".into())
            } else {
                Ok(self.corrected_start.unwrap_or(requested))
            }
        }

        fn flush_in_place(&mut self) -> Result<(), String> {
            self.calls.push("ap2_flush");
            if self.fail_flush {
                Err("ap2 flush failed".into())
            } else {
                Ok(())
            }
        }

        fn warm_head_unix_ms(&self) -> Option<u64> {
            self.warm_head
        }

        fn standby(&mut self) -> Result<(), String> {
            self.calls.push("ap2_standby");
            if self.fail_standby {
                Err("ap2 standby failed".into())
            } else {
                Ok(())
            }
        }
    }

    fn session(client: FakeAp2Client) -> MsaSession<MsaAp2Transport<FakeAp2Client>> {
        MsaSession::new(
            MsaSessionCore::new(176_400, 1_408).unwrap(),
            MsaAp2Transport::new(client),
        )
    }

    #[test]
    fn start_returns_ap2_corrected_truth_not_requested_guess() {
        let mut s = session(FakeAp2Client {
            corrected_start: Some(12_345),
            ..Default::default()
        });
        let ack = s.start(12_000).unwrap();
        assert_eq!(ack.requested_unix_ms, 12_000);
        assert_eq!(ack.committed_unix_ms, 12_345);
        assert_eq!(s.core().state(), MsaSessionState::Playing);
        assert!(!s.transport().is_quiesced());
        assert_eq!(s.transport().client().calls, vec!["ap2_start"]);
    }

    #[test]
    fn flush_reports_frozen_warm_head_and_returns_idle() {
        let mut s = session(FakeAp2Client {
            warm_head: Some(55_000),
            ..Default::default()
        });
        s.start(10_000).unwrap();
        let ack = s.flush().unwrap();

        assert_eq!(ack.warm_head_unix_ms, Some(55_000));
        assert_eq!(s.core().state(), MsaSessionState::Idle);
        assert_eq!(
            s.transport().client().calls,
            vec!["ap2_start", "ap2_flush"]
        );
        assert!(!s.transport().is_quiesced());
    }

    #[test]
    fn standby_parks_without_disconnect_contract() {
        let mut s = session(FakeAp2Client::default());
        s.start(10_000).unwrap();
        s.standby().unwrap();

        assert_eq!(s.core().state(), MsaSessionState::Standby);
        assert_eq!(
            s.transport().client().calls,
            vec!["ap2_start", "ap2_standby"]
        );
        assert!(!s.transport().is_quiesced());
    }

    #[test]
    fn transport_commands_cannot_bypass_quiesce() {
        let mut transport = MsaAp2Transport::new(FakeAp2Client::default());

        assert!(transport.commit_start(1).is_err());
        assert!(transport.flush().is_err());
        assert!(transport.stop().is_err());

        transport.quiesce().unwrap();
        assert_eq!(transport.commit_start(1).unwrap(), 1);
        transport.resume().unwrap();
    }

    #[test]
    fn failed_ap2_start_is_not_published_as_playing() {
        let mut s = session(FakeAp2Client {
            fail_start: true,
            ..Default::default()
        });
        assert!(s.start(10_000).is_err());
        assert_eq!(s.core().state(), MsaSessionState::Idle);
        assert_eq!(s.core().epoch(), 0);
        assert!(!s.transport().is_quiesced());
    }
}
