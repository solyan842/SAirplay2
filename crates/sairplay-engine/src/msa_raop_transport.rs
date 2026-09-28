//! RAOP transport adapter for the SAirplay 2.0 MSA clone.
//!
//! Mirrors pinned Music Assistant `raop_session.c` semantics:
//! - feasible non-zero START is honored exactly;
//! - infeasible non-zero START is corrected forward with one extra 200 ms lead;
//! - START=0 uses the feasibility floor;
//! - audible START is converted to libraop start time by subtracting receiver latency;
//! - warm START is only valid from FLUSHED and does not flush again;
//! - FLUSH/STANDBY preserve the connected client.

use crate::MsaSessionTransport;

pub const MSA_RAOP_MIN_START_LEAD_MS: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaRaopState {
    Streaming,
    Flushed,
    Down,
}

pub trait MsaRaopClient {
    fn state(&self) -> MsaRaopState;
    fn now_unix_ms(&self) -> u64;
    fn latency_frames(&self) -> u32;
    fn sample_rate(&self) -> u32;

    fn stop(&mut self);
    fn flush(&mut self) -> Result<(), String>;
    fn start_transport_at_unix_ms(&mut self, transport_start_unix_ms: u64)
        -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaRaopStartMode {
    InitialCommit,
    WarmStartAfterFlush,
}

pub struct MsaRaopTransport<C: MsaRaopClient> {
    client: C,
    quiesced: bool,
    start_mode: MsaRaopStartMode,
}

impl<C: MsaRaopClient> MsaRaopTransport<C> {
    pub fn new(client: C, start_mode: MsaRaopStartMode) -> Self {
        Self {
            client,
            quiesced: false,
            start_mode,
        }
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }

    pub fn start_mode(&self) -> MsaRaopStartMode {
        self.start_mode
    }

    pub fn set_start_mode(&mut self, mode: MsaRaopStartMode) {
        self.start_mode = mode;
    }

    pub fn is_quiesced(&self) -> bool {
        self.quiesced
    }

    fn require_quiesced(&self, operation: &str) -> Result<(), String> {
        if self.quiesced {
            Ok(())
        } else {
            Err(format!(
                "MSA RAOP transport {operation} requires quiesced send path"
            ))
        }
    }

    fn resolve_audible_start_unix_ms(&self, requested: u64) -> u64 {
        let floor = self
            .client
            .now_unix_ms()
            .saturating_add(MSA_RAOP_MIN_START_LEAD_MS);

        if requested == 0 {
            return floor;
        }
        if requested >= floor {
            return requested;
        }

        floor.saturating_add(MSA_RAOP_MIN_START_LEAD_MS)
    }

    fn latency_ms(&self) -> u64 {
        let rate = self.client.sample_rate() as u64;
        if rate == 0 {
            return 0;
        }
        (self.client.latency_frames() as u64)
            .saturating_mul(1000)
            / rate
    }

    fn commit_initial(&mut self, requested: u64) -> Result<u64, String> {
        let state = self.client.state();
        if !matches!(state, MsaRaopState::Streaming | MsaRaopState::Flushed) {
            return Err("MSA RAOP initial commit requires STREAMING or FLUSHED".into());
        }

        let audible = self.resolve_audible_start_unix_ms(requested);

        // MSA raop_session_commit: always stop; flush only if the old stream
        // was live; then start at audible minus receiver latency.
        self.client.stop();
        if state == MsaRaopState::Streaming {
            self.client.flush()?;
        }

        let transport_start = audible.saturating_sub(self.latency_ms());
        self.client.start_transport_at_unix_ms(transport_start)?;
        Ok(audible)
    }

    fn commit_warm(&mut self, requested: u64) -> Result<u64, String> {
        if self.client.state() != MsaRaopState::Flushed {
            return Err(
                "MSA RAOP warm START requires FLUSHED state; live re-anchor rejected".into(),
            );
        }

        let audible = self.resolve_audible_start_unix_ms(requested);
        let transport_start = audible.saturating_sub(self.latency_ms());

        // No stop/flush here: raop_session_start_at explicitly reuses the
        // already-flushed transport state.
        self.client.start_transport_at_unix_ms(transport_start)?;
        Ok(audible)
    }
}

impl<C: MsaRaopClient> MsaSessionTransport for MsaRaopTransport<C> {
    fn quiesce(&mut self) -> Result<(), String> {
        if self.quiesced {
            return Err("MSA RAOP transport already quiesced".into());
        }
        self.quiesced = true;
        Ok(())
    }

    fn commit_start(&mut self, requested_start_unix_ms: u64) -> Result<u64, String> {
        self.require_quiesced("START")?;
        match self.start_mode {
            MsaRaopStartMode::InitialCommit => {
                self.commit_initial(requested_start_unix_ms)
            }
            MsaRaopStartMode::WarmStartAfterFlush => {
                self.commit_warm(requested_start_unix_ms)
            }
        }
    }

    fn flush(&mut self) -> Result<Option<u64>, String> {
        self.require_quiesced("FLUSH")?;
        match self.client.state() {
            MsaRaopState::Streaming => {
                self.client.stop();
                self.client.flush()?;
            }
            MsaRaopState::Flushed => {
                self.client.stop();
            }
            MsaRaopState::Down => {
                return Err("MSA RAOP FLUSH requires STREAMING or FLUSHED".into())
            }
        }
        self.start_mode = MsaRaopStartMode::WarmStartAfterFlush;
        Ok(None)
    }

    fn stop(&mut self) -> Result<(), String> {
        self.require_quiesced("STANDBY")?;
        // MSA raop_session_standby is exactly the in-place flush contract.
        match self.client.state() {
            MsaRaopState::Streaming => {
                self.client.stop();
                self.client.flush()?;
            }
            MsaRaopState::Flushed => {
                self.client.stop();
            }
            MsaRaopState::Down => {
                return Err("MSA RAOP STANDBY requires STREAMING or FLUSHED".into())
            }
        }
        self.start_mode = MsaRaopStartMode::WarmStartAfterFlush;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), String> {
        if !self.quiesced {
            return Err("MSA RAOP transport resume without quiesce".into());
        }
        self.quiesced = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MsaSession, MsaSessionCore, MsaSessionState};

    struct FakeRaop {
        state: MsaRaopState,
        now_ms: u64,
        latency_frames: u32,
        sample_rate: u32,
        calls: Vec<&'static str>,
        last_transport_start_ms: Option<u64>,
    }

    impl FakeRaop {
        fn flushed() -> Self {
            Self {
                state: MsaRaopState::Flushed,
                now_ms: 1_000,
                latency_frames: 11_025,
                sample_rate: 44_100,
                calls: Vec::new(),
                last_transport_start_ms: None,
            }
        }
    }

    impl MsaRaopClient for FakeRaop {
        fn state(&self) -> MsaRaopState { self.state }
        fn now_unix_ms(&self) -> u64 { self.now_ms }
        fn latency_frames(&self) -> u32 { self.latency_frames }
        fn sample_rate(&self) -> u32 { self.sample_rate }

        fn stop(&mut self) {
            self.calls.push("stop");
            self.state = MsaRaopState::Flushed;
        }

        fn flush(&mut self) -> Result<(), String> {
            self.calls.push("flush");
            self.state = MsaRaopState::Flushed;
            Ok(())
        }

        fn start_transport_at_unix_ms(&mut self, at: u64) -> Result<(), String> {
            self.calls.push("start");
            self.last_transport_start_ms = Some(at);
            self.state = MsaRaopState::Streaming;
            Ok(())
        }
    }

    fn session(
        client: FakeRaop,
        mode: MsaRaopStartMode,
    ) -> MsaSession<MsaRaopTransport<FakeRaop>> {
        MsaSession::new(
            MsaSessionCore::new(176_400, 1_408).unwrap(),
            MsaRaopTransport::new(client, mode),
        )
    }

    #[test]
    fn feasible_initial_start_is_exact_and_latency_compensated() {
        let mut s = session(FakeRaop::flushed(), MsaRaopStartMode::InitialCommit);
        let ack = s.start(2_000).unwrap();

        assert_eq!(ack.committed_unix_ms, 2_000);
        // 11025 / 44100 = 250 ms receiver latency.
        assert_eq!(
            s.transport().client().last_transport_start_ms,
            Some(1_750)
        );
        assert_eq!(
            s.transport().client().calls,
            vec!["stop", "start"]
        );
        assert_eq!(s.core().state(), MsaSessionState::Playing);
    }

    #[test]
    fn infeasible_nonzero_start_moves_forward_with_retry_slack() {
        let mut s = session(FakeRaop::flushed(), MsaRaopStartMode::InitialCommit);
        let ack = s.start(1_050).unwrap();

        // floor = 1200, corrected non-zero = floor + 200 = 1400.
        assert_eq!(ack.committed_unix_ms, 1_400);
        assert_eq!(
            s.transport().client().last_transport_start_ms,
            Some(1_150)
        );
    }

    #[test]
    fn zero_start_uses_floor_without_extra_slack() {
        let mut s = session(FakeRaop::flushed(), MsaRaopStartMode::InitialCommit);
        let ack = s.start(0).unwrap();

        assert_eq!(ack.committed_unix_ms, 1_200);
        assert_eq!(
            s.transport().client().last_transport_start_ms,
            Some(950)
        );
    }

    #[test]
    fn initial_commit_flushes_only_when_already_streaming() {
        let mut client = FakeRaop::flushed();
        client.state = MsaRaopState::Streaming;
        let mut s = session(client, MsaRaopStartMode::InitialCommit);

        s.start(2_000).unwrap();

        assert_eq!(
            s.transport().client().calls,
            vec!["stop", "flush", "start"]
        );
    }

    #[test]
    fn warm_start_after_flush_does_not_flush_again() {
        let mut s = session(
            FakeRaop::flushed(),
            MsaRaopStartMode::WarmStartAfterFlush,
        );

        s.start(2_000).unwrap();

        assert_eq!(s.transport().client().calls, vec!["start"]);
    }

    #[test]
    fn warm_start_on_live_unflushed_stream_fails_fast() {
        let mut client = FakeRaop::flushed();
        client.state = MsaRaopState::Streaming;
        let mut s = session(client, MsaRaopStartMode::WarmStartAfterFlush);

        assert!(s.start(2_000).is_err());
        assert_eq!(s.core().state(), MsaSessionState::Idle);
        assert_eq!(s.core().epoch(), 0);
        assert!(s.transport().client().calls.is_empty());
    }

    #[test]
    fn flush_keeps_connection_reusable_for_warm_start() {
        let mut client = FakeRaop::flushed();
        client.state = MsaRaopState::Streaming;
        let mut s = session(client, MsaRaopStartMode::InitialCommit);

        s.start(2_000).unwrap();
        s.transport_mut().client_mut().calls.clear();
        s.flush().unwrap();

        assert_eq!(s.core().state(), MsaSessionState::Idle);
        assert_eq!(
            s.transport().start_mode(),
            MsaRaopStartMode::WarmStartAfterFlush
        );
        assert_eq!(
            s.transport().client().calls,
            vec!["stop", "flush"]
        );
    }
}
