use crate::StreamPorts;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum MediaTransportError {
    BindData(io::Error),
    BindControl(io::Error),
    Configure(io::Error),
    LocalAddr(io::Error),
    RemoteNotAttached,
    SendData(io::Error),
    SendControl(io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTransportPorts {
    pub data_port: u16,
    pub control_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteMediaEndpoints {
    pub data: SocketAddr,
    pub control: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramSendOutcome {
    Sent(usize),
    Dropped,
}

pub struct MediaTransport {
    data_socket: UdpSocket,
    control_socket: UdpSocket,
    remote: Option<RemoteMediaEndpoints>,
}

impl MediaTransport {
    pub fn bind(bind_ip: IpAddr) -> Result<Self, MediaTransportError> {
        let data_socket =
            UdpSocket::bind(SocketAddr::new(bind_ip, 0)).map_err(MediaTransportError::BindData)?;
        let control_socket =
            UdpSocket::bind(SocketAddr::new(bind_ip, 0)).map_err(MediaTransportError::BindControl)?;

        data_socket
            .set_nonblocking(true)
            .map_err(MediaTransportError::Configure)?;
        control_socket
            .set_nonblocking(true)
            .map_err(MediaTransportError::Configure)?;

        Ok(Self {
            data_socket,
            control_socket,
            remote: None,
        })
    }

    pub fn local_ports(&self) -> Result<MediaTransportPorts, MediaTransportError> {
        let data_port = self
            .data_socket
            .local_addr()
            .map_err(MediaTransportError::LocalAddr)?
            .port();
        let control_port = self
            .control_socket
            .local_addr()
            .map_err(MediaTransportError::LocalAddr)?
            .port();

        Ok(MediaTransportPorts {
            data_port,
            control_port,
        })
    }

    pub fn attach_remote(&mut self, receiver_ip: IpAddr, ports: StreamPorts) {
        self.remote = Some(RemoteMediaEndpoints {
            data: SocketAddr::new(receiver_ip, ports.data_port),
            control: SocketAddr::new(receiver_ip, ports.control_port),
        });
    }

    pub fn remote_endpoints(&self) -> Option<RemoteMediaEndpoints> {
        self.remote
    }

    pub fn clone_control_socket(&self) -> io::Result<UdpSocket> {
        self.control_socket.try_clone()
    }

    pub fn send_data_deadline(
        &self,
        packet: &[u8],
        timeout: Duration,
    ) -> Result<DatagramSendOutcome, MediaTransportError> {
        let remote = self.remote.ok_or(MediaTransportError::RemoteNotAttached)?;
        send_datagram_deadline(
            &self.data_socket,
            packet,
            remote.data,
            timeout,
            MediaTransportError::SendData,
        )
    }

    pub fn send_control_deadline(
        &self,
        packet: &[u8],
        timeout: Duration,
    ) -> Result<DatagramSendOutcome, MediaTransportError> {
        let remote = self.remote.ok_or(MediaTransportError::RemoteNotAttached)?;
        send_datagram_deadline(
            &self.control_socket,
            packet,
            remote.control,
            timeout,
            MediaTransportError::SendControl,
        )
    }

    pub fn send_data(&self, packet: &[u8]) -> Result<usize, MediaTransportError> {
        let remote = self.remote.ok_or(MediaTransportError::RemoteNotAttached)?;
        self.data_socket
            .send_to(packet, remote.data)
            .map_err(MediaTransportError::SendData)
    }

    pub fn send_control(&self, packet: &[u8]) -> Result<usize, MediaTransportError> {
        let remote = self.remote.ok_or(MediaTransportError::RemoteNotAttached)?;
        self.control_socket
            .send_to(packet, remote.control)
            .map_err(MediaTransportError::SendControl)
    }
}

fn send_datagram_deadline(
    socket: &UdpSocket,
    packet: &[u8],
    remote: SocketAddr,
    timeout: Duration,
    map_error: fn(io::Error) -> MediaTransportError,
) -> Result<DatagramSendOutcome, MediaTransportError> {
    let deadline = Instant::now() + timeout;
    loop {
        match socket.send_to(packet, remote) {
            Ok(n) => return Ok(DatagramSendOutcome::Sent(n)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() >= deadline {
                    return Ok(DatagramSendOutcome::Dropped);
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(map_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::time::Duration;

    #[test]
    fn binds_two_real_udp_ports_before_stream_setup() {
        let transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let ports = transport.local_ports().unwrap();

        assert_ne!(ports.data_port, 0);
        assert_ne!(ports.control_port, 0);
        assert_ne!(ports.data_port, ports.control_port);
        assert!(transport.remote_endpoints().is_none());
    }

    #[test]
    fn advertised_local_ports_remain_bound_while_transport_is_alive() {
        let transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let ports = transport.local_ports().unwrap();

        assert!(UdpSocket::bind((Ipv4Addr::LOCALHOST, ports.data_port)).is_err());
        assert!(UdpSocket::bind((Ipv4Addr::LOCALHOST, ports.control_port)).is_err());
    }

    #[test]
    fn remote_ports_attach_by_role_after_setup_response() {
        let mut transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        transport.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            StreamPorts {
                data_port: 61000,
                control_port: 61001,
            },
        );

        assert_eq!(
            transport.remote_endpoints(),
            Some(RemoteMediaEndpoints {
                data: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 61000),
                control: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 61001),
            })
        );
    }

    #[test]
    fn cannot_send_before_remote_setup_is_attached() {
        let transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert!(matches!(
            transport.send_data(b"rtp"),
            Err(MediaTransportError::RemoteNotAttached)
        ));
        assert!(matches!(
            transport.send_control(b"sync"),
            Err(MediaTransportError::RemoteNotAttached)
        ));
    }

    #[test]
    fn data_and_control_packets_use_their_own_remote_ports() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let mut transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        transport.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            StreamPorts {
                data_port: data_rx.local_addr().unwrap().port(),
                control_port: ctrl_rx.local_addr().unwrap().port(),
            },
        );

        transport.send_data(b"data-packet").unwrap();
        transport.send_control(b"control-packet").unwrap();

        let mut buf = [0u8; 64];
        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..dn], b"data-packet");

        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..cn], b"control-packet");
    }
}
