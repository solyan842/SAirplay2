use crate::{NativeConnectError, NativeConnectFlow, NtpTimingError, NtpTimingResponder};
use std::net::SocketAddr;

#[derive(Debug)]
pub enum NativeTimingGateError {
    Flow(NativeConnectError),
    Ntp(NtpTimingError),
}

impl From<NativeConnectError> for NativeTimingGateError {
    fn from(value: NativeConnectError) -> Self {
        Self::Flow(value)
    }
}

impl From<NtpTimingError> for NativeTimingGateError {
    fn from(value: NtpTimingError) -> Self {
        Self::Ntp(value)
    }
}

/// Start the NTP timing responder and advance the native connect state only
/// after the UDP timing service is actually live.
///
/// Invariant:
/// - On success: flow is TimingReady and returned responder is running.
/// - On bind/start failure: flow remains Paired.
pub fn start_ntp_timing_gate(
    flow: &mut NativeConnectFlow,
    bind_addr: SocketAddr,
) -> Result<NtpTimingResponder, NativeTimingGateError> {
    // Verify ordering before allocating network resources.
    if flow.phase() != crate::NativePhase::Paired {
        // Reuse the state-machine's own transition error without mutating it.
        flow.timing_ready()?;
        unreachable!("timing_ready succeeds only from Paired");
    }

    let mut responder = NtpTimingResponder::bind(bind_addr)?;
    responder.start()?;

    // From this point timing service is live, so the state may advance.
    flow.timing_ready()?;
    Ok(responder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativePhase;
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};

    fn paired_flow() -> NativeConnectFlow {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();
        flow.paired().unwrap();
        flow
    }

    #[test]
    fn successful_ntp_start_advances_paired_to_timing_ready() {
        let mut flow = paired_flow();
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

        let mut responder = start_ntp_timing_gate(&mut flow, bind).unwrap();

        assert_eq!(flow.phase(), NativePhase::TimingReady);
        assert!(responder.is_running());
        assert_ne!(responder.port().unwrap(), 0);

        responder.stop();
    }

    #[test]
    fn bind_failure_keeps_flow_at_paired() {
        let occupied = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = occupied.local_addr().unwrap();

        let mut flow = paired_flow();
        let result = start_ntp_timing_gate(&mut flow, addr);

        assert!(matches!(result, Err(NativeTimingGateError::Ntp(_))));
        assert_eq!(flow.phase(), NativePhase::Paired);
    }

    #[test]
    fn timing_gate_rejects_wrong_connect_phase_without_starting_service() {
        let mut flow = NativeConnectFlow::default();
        flow.tcp_connected().unwrap();
        flow.info_loaded().unwrap();

        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let result = start_ntp_timing_gate(&mut flow, bind);

        assert!(matches!(result, Err(NativeTimingGateError::Flow(_))));
        assert_eq!(flow.phase(), NativePhase::InfoLoaded);
    }

    #[test]
    fn session_setup_remains_blocked_when_ntp_cannot_start() {
        let occupied = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = occupied.local_addr().unwrap();

        let mut flow = paired_flow();
        assert!(start_ntp_timing_gate(&mut flow, addr).is_err());

        assert_eq!(
            flow.session_setup(),
            Err(NativeConnectError {
                expected: NativePhase::TimingReady,
                actual: NativePhase::Paired,
            })
        );
    }
}
