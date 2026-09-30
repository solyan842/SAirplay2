#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePhase {
    Down,
    TcpConnected,
    InfoLoaded,
    Paired,
    TimingReady,
    SessionSetup,
    EventChannelOpen,
    Recorded,
    StreamSetup,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeConnectError {
    pub expected: NativePhase,
    pub actual: NativePhase,
}

#[derive(Debug, Clone)]
pub struct NativeConnectFlow {
    phase: NativePhase,
}

impl Default for NativeConnectFlow {
    fn default() -> Self {
        Self {
            phase: NativePhase::Down,
        }
    }
}

impl NativeConnectFlow {
    pub fn phase(&self) -> NativePhase {
        self.phase
    }

    fn step(
        &mut self,
        expected: NativePhase,
        next: NativePhase,
    ) -> Result<(), NativeConnectError> {
        if self.phase != expected {
            return Err(NativeConnectError {
                expected,
                actual: self.phase,
            });
        }
        self.phase = next;
        Ok(())
    }

    pub fn tcp_connected(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::Down, NativePhase::TcpConnected)
    }

    pub fn info_loaded(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::TcpConnected, NativePhase::InfoLoaded)
    }

    pub fn paired(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::InfoLoaded, NativePhase::Paired)
    }

    pub fn timing_ready(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::Paired, NativePhase::TimingReady)
    }

    pub fn session_setup(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::TimingReady, NativePhase::SessionSetup)
    }

    pub fn event_channel_open(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::SessionSetup, NativePhase::EventChannelOpen)
    }

    pub fn recorded(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::EventChannelOpen, NativePhase::Recorded)
    }

    pub fn stream_setup(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::Recorded, NativePhase::StreamSetup)
    }

    pub fn ready(&mut self) -> Result<(), NativeConnectError> {
        self.step(NativePhase::StreamSetup, NativePhase::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_native_connect_order_reaches_ready() {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow.session_setup().unwrap();
        flow.event_channel_open().unwrap();
        flow.recorded().unwrap();
        flow.stream_setup().unwrap();
        flow.ready().unwrap();
        assert_eq!(flow.phase(), NativePhase::Ready);
    }

    #[test]
    fn record_must_precede_stream_setup() {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow.timing_ready().unwrap();
        flow.session_setup().unwrap();
        flow.event_channel_open().unwrap();

        assert_eq!(
            flow.stream_setup(),
            Err(NativeConnectError {
                expected: NativePhase::Recorded,
                actual: NativePhase::EventChannelOpen,
            })
        );

        flow.recorded().unwrap();
        flow.stream_setup().unwrap();
    }

    #[test]
    fn encrypted_session_setup_cannot_happen_before_pairing_and_timing() {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();

        assert_eq!(
            flow.session_setup(),
            Err(NativeConnectError {
                expected: NativePhase::TimingReady,
                actual: NativePhase::InfoLoaded,
            })
        );
    }
}
