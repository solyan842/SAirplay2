//! Independent Music Assistant compatible SOLO engine.
//! Source: music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128
//! Legacy SAirplay Solo/MultiRoom remain frozen.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState { Idle, Playing, Standby, Ended }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route { Raop, AirPlay2Compat, AirPlay2Native }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartAck { pub requested_unix_ms: u64, pub at_unix_ms: u64 }

pub trait SoloTransport {
    type Error;
    fn quiesce(&mut self);
    fn flush(&mut self) -> Result<(), Self::Error>;
    fn commit_start(&mut self, requested_unix_ms: u64) -> Result<u64, Self::Error>;
    fn resume(&mut self);
    fn standby(&mut self) -> Result<(), Self::Error>;
    fn disconnect(&mut self);
}

pub struct SoloSession<T: SoloTransport> {
    transport: T,
    state: SessionState,
    epoch: u64,
}

impl<T: SoloTransport> SoloSession<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, state: SessionState::Idle, epoch: 0 }
    }
    pub fn state(&self) -> SessionState { self.state }
    pub fn epoch(&self) -> u64 { self.epoch }

    pub fn start(&mut self, requested_unix_ms: u64) -> Result<StartAck, T::Error> {
        self.transport.quiesce();
        let result = self.transport.commit_start(requested_unix_ms);
        self.transport.resume();
        let at_unix_ms = result?;
        self.state = SessionState::Playing;
        self.epoch = self.epoch.wrapping_add(1);
        Ok(StartAck { requested_unix_ms, at_unix_ms })
    }

    pub fn flush(&mut self) -> Result<(), T::Error> {
        self.transport.quiesce();
        let result = self.transport.flush();
        self.transport.resume();
        result?;
        self.state = SessionState::Idle;
        Ok(())
    }

    pub fn standby(&mut self) -> Result<(), T::Error> {
        self.transport.quiesce();
        let result = self.transport.standby();
        self.transport.resume();
        result?;
        self.state = SessionState::Standby;
        Ok(())
    }

    pub fn end(&mut self) {
        self.transport.disconnect();
        self.state = SessionState::Ended;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport { scheduled: u64 }

    impl SoloTransport for FakeTransport {
        type Error = &'static str;
        fn quiesce(&mut self) {}
        fn flush(&mut self) -> Result<(), Self::Error> { Ok(()) }
        fn commit_start(&mut self, requested: u64) -> Result<u64, Self::Error> {
            Ok(if self.scheduled == 0 { requested } else { self.scheduled })
        }
        fn resume(&mut self) {}
        fn standby(&mut self) -> Result<(), Self::Error> { Ok(()) }
        fn disconnect(&mut self) {}
    }

    #[test]
    fn lifecycle_matches_msa_command_shape() {
        let mut session = SoloSession::new(FakeTransport { scheduled: 1234 });
        let ack = session.start(1000).unwrap();
        assert_eq!(ack, StartAck { requested_unix_ms: 1000, at_unix_ms: 1234 });
        assert_eq!(session.state(), SessionState::Playing);
        assert_eq!(session.epoch(), 1);
        session.flush().unwrap();
        assert_eq!(session.state(), SessionState::Idle);
        session.start(2000).unwrap();
        assert_eq!(session.epoch(), 2);
        session.standby().unwrap();
        assert_eq!(session.state(), SessionState::Standby);
        session.end();
        assert_eq!(session.state(), SessionState::Ended);
    }
}
