use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NTP_EPOCH_DELTA: u64 = 2_208_988_800;

#[derive(Debug)]
pub enum NtpTimingError {
    Bind(io::Error),
    Configure(io::Error),
    Time,
}

pub struct NtpTimingResponder {
    socket: UdpSocket,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl NtpTimingResponder {
    pub fn bind(bind_addr: SocketAddr) -> Result<Self, NtpTimingError> {
        let socket = UdpSocket::bind(bind_addr).map_err(NtpTimingError::Bind)?;
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(NtpTimingError::Configure)?;

        Ok(Self {
            socket,
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn port(&self) -> io::Result<u16> {
        Ok(self.local_addr()?.port())
    }

    pub fn start(&mut self) -> Result<(), NtpTimingError> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let socket = self.socket.try_clone().map_err(NtpTimingError::Configure)?;
        let running = Arc::clone(&self.running);

        self.worker = Some(thread::spawn(move || {
            let mut request = [0u8; 256];

            while running.load(Ordering::SeqCst) {
                match socket.recv_from(&mut request) {
                    Ok((n, peer)) => {
                        if n != 32 {
                            continue;
                        }

                        let Ok(receive_ntp) = system_time_to_ntp(SystemTime::now()) else {
                            continue;
                        };

                        let Some(mut response) =
                            build_timing_response(&request[..n], receive_ntp, receive_ntp)
                        else {
                            continue;
                        };

                        if let Ok(transmit_ntp) = system_time_to_ntp(SystemTime::now()) {
                            response[24..32].copy_from_slice(&transmit_ntp.to_be_bytes());
                        }

                        let _ = socket.send_to(&response, peer);
                    }
                    Err(err)
                        if err.kind() == io::ErrorKind::WouldBlock
                            || err.kind() == io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }));

        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        // Wake a blocking recv_from immediately instead of waiting for timeout.
        if let Ok(addr) = self.socket.local_addr() {
            if let Ok(wake) = UdpSocket::bind(("127.0.0.1", 0)) {
                let _ = wake.send_to(&[0u8], addr);
            }
        }

        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

impl Drop for NtpTimingResponder {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn system_time_to_ntp(time: SystemTime) -> Result<u64, NtpTimingError> {
    let since_unix = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NtpTimingError::Time)?;

    let seconds = since_unix
        .as_secs()
        .checked_add(NTP_EPOCH_DELTA)
        .ok_or(NtpTimingError::Time)?;

    let fraction = ((since_unix.subsec_nanos() as u128) << 32) / 1_000_000_000u128;

    Ok((seconds << 32) | fraction as u64)
}

pub fn build_timing_response(
    request: &[u8],
    receive_ntp: u64,
    transmit_ntp: u64,
) -> Option<[u8; 32]> {
    if request.len() != 32 || request[0] != 0x80 || request[1] != 0xD2 {
        return None;
    }

    let mut response = [0u8; 32];
    response[0] = 0x80;
    response[1] = 0xD3;
    response[2] = request[2];

    // Originate/reference timestamp = sender timestamp from request.
    response[8..16].copy_from_slice(&request[24..32]);

    // Receive and transmit timestamps are NTP fixed-point, big endian.
    response[16..24].copy_from_slice(&receive_ntp.to_be_bytes());
    response[24..32].copy_from_slice(&transmit_ntp.to_be_bytes());

    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn packet_shape_matches_airplay_timing_reference() {
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xD2;
        request[2] = 0x44;
        request[24..32].copy_from_slice(&0x0102030405060708u64.to_be_bytes());

        let response = build_timing_response(
            &request,
            0x1112131415161718,
            0x2122232425262728,
        )
        .unwrap();

        assert_eq!(response[0], 0x80);
        assert_eq!(response[1], 0xD3);
        assert_eq!(response[2], 0x44);
        assert_eq!(&response[8..16], &request[24..32]);
        assert_eq!(
            u64::from_be_bytes(response[16..24].try_into().unwrap()),
            0x1112131415161718
        );
        assert_eq!(
            u64::from_be_bytes(response[24..32].try_into().unwrap()),
            0x2122232425262728
        );
    }

    #[test]
    fn malformed_or_wrong_timing_request_is_ignored() {
        assert!(build_timing_response(&[0u8; 31], 1, 2).is_none());

        let mut wrong = [0u8; 32];
        wrong[0] = 0x80;
        wrong[1] = 0xD3;
        assert!(build_timing_response(&wrong, 1, 2).is_none());
    }

    #[test]
    fn unix_epoch_converts_to_ntp_epoch_delta() {
        let ntp = system_time_to_ntp(UNIX_EPOCH).unwrap();
        assert_eq!(ntp >> 32, NTP_EPOCH_DELTA);
        assert_eq!(ntp as u32, 0);
    }

    #[test]
    fn udp_loopback_answers_real_timing_request() {
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let mut responder = NtpTimingResponder::bind(bind).unwrap();
        responder.start().unwrap();

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xD2;
        request[2] = 0x7A;
        let client_send = 0xA1A2A3A4A5A6A7A8u64;
        request[24..32].copy_from_slice(&client_send.to_be_bytes());

        client.send_to(&request, responder.local_addr().unwrap()).unwrap();

        let mut response = [0u8; 32];
        let (n, _) = client.recv_from(&mut response).unwrap();

        assert_eq!(n, 32);
        assert_eq!(&response[..3], &[0x80, 0xD3, 0x7A]);
        assert_eq!(
            u64::from_be_bytes(response[8..16].try_into().unwrap()),
            client_send
        );

        let receive = u64::from_be_bytes(response[16..24].try_into().unwrap());
        let transmit = u64::from_be_bytes(response[24..32].try_into().unwrap());
        assert!(receive > 0);
        assert!(transmit >= receive);

        responder.stop();
        assert!(!responder.is_running());
    }
}
