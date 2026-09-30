//! Concrete native AP2 media sockets for SOLO.
//! UDP deadline/drop behavior and buffered TCP pending semantics mirror pinned MSA.

use crate::native_media::{MediaIo, SendResult, StreamWrite};
use crate::native_rtx::RtxIo;
use crate::native_sync::SyncIo;
use std::io::{self, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

pub const UDP_SEND_TIMEOUT: Duration = Duration::from_millis(20);
pub const BUFFERED_WRITE_TIMEOUT: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalMediaPorts {
    pub data: u16,
    pub control: u16,
}

pub struct NativeMediaIo {
    data_socket: UdpSocket,
    control_socket: UdpSocket,
    remote_data: Option<SocketAddr>,
    remote_control: Option<SocketAddr>,
    buffered: Option<TcpStream>,
}

impl NativeMediaIo {
    pub fn bind(bind_ip: IpAddr) -> io::Result<Self> {
        let data_socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0))?;
        let control_socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0))?;
        data_socket.set_nonblocking(true)?;
        control_socket.set_nonblocking(true)?;
        Ok(Self {
            data_socket,
            control_socket,
            remote_data: None,
            remote_control: None,
            buffered: None,
        })
    }

    pub fn local_ports(&self) -> io::Result<LocalMediaPorts> {
        Ok(LocalMediaPorts {
            data: self.data_socket.local_addr()?.port(),
            control: self.control_socket.local_addr()?.port(),
        })
    }

    pub fn attach_data_remote(&mut self, receiver_ip: IpAddr, data_port: u16) {
        self.remote_data = Some(SocketAddr::new(receiver_ip, data_port));
    }

    pub fn attach_control_remote(&mut self, receiver_ip: IpAddr, control_port: u16) {
        self.remote_control = Some(SocketAddr::new(receiver_ip, control_port));
    }

    pub fn attach_remote(&mut self, receiver_ip: IpAddr, data_port: u16, control_port: u16) {
        self.attach_data_remote(receiver_ip, data_port);
        self.attach_control_remote(receiver_ip, control_port);
    }

    pub fn connect_buffered(&mut self) -> io::Result<()> {
        let remote = self.remote_data.ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "remote data endpoint not attached"))?;
        let stream = TcpStream::connect_timeout(&remote, BUFFERED_WRITE_TIMEOUT)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(BUFFERED_WRITE_TIMEOUT))?;
        self.buffered = Some(stream);
        Ok(())
    }

    pub fn clone_control_socket(&self) -> io::Result<UdpSocket> {
        self.control_socket.try_clone()
    }

    pub fn recv_control_nonblocking(&self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        match self.control_socket.recv_from(buf) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn send_data_deadline(&self, packet: &[u8]) -> SendResult {
        let remote = match self.remote_data {
            Some(v) => v,
            None => return SendResult::Fatal,
        };
        send_datagram_deadline(&self.data_socket, packet, remote, UDP_SEND_TIMEOUT)
    }

    fn send_control_deadline_to(&self, packet: &[u8], remote: SocketAddr) -> SendResult {
        send_datagram_deadline(&self.control_socket, packet, remote, UDP_SEND_TIMEOUT)
    }

    fn send_control_deadline(&self, packet: &[u8]) -> SendResult {
        let remote = match self.remote_control {
            Some(v) => v,
            None => return SendResult::Fatal,
        };
        self.send_control_deadline_to(packet, remote)
    }
}

fn transient_udp(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(10055) {
        return true;
    }
    false
}

pub(crate) fn send_datagram_deadline(
    socket: &UdpSocket,
    packet: &[u8],
    remote: SocketAddr,
    timeout: Duration,
) -> SendResult {
    let deadline = Instant::now() + timeout;
    loop {
        match socket.send_to(packet, remote) {
            Ok(n) if n == packet.len() => return SendResult::Sent,
            Ok(_) => return SendResult::Fatal,
            Err(e) if transient_udp(&e) => {
                if Instant::now() >= deadline {
                    return SendResult::Dropped;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return SendResult::Fatal,
        }
    }
}

impl MediaIo for NativeMediaIo {
    fn send_realtime(&mut self, packet: &[u8]) -> SendResult {
        self.send_data_deadline(packet)
    }

    fn send_buffered(&mut self, bytes: &[u8]) -> StreamWrite {
        let stream = match self.buffered.as_mut() {
            Some(v) => v,
            None => return StreamWrite::Fatal,
        };
        loop {
            match stream.write(bytes) {
                Ok(n) if n == bytes.len() => return StreamWrite::Complete,
                Ok(n) if n > 0 => return StreamWrite::Partial(n),
                Ok(_) => return StreamWrite::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return StreamWrite::WouldBlock;
                }
                Err(_) => return StreamWrite::Fatal,
            }
        }
    }

    fn close_buffered(&mut self) {
        if let Some(stream) = self.buffered.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

impl SyncIo for NativeMediaIo {
    fn send_sync(&mut self, packet: &[u8]) -> SendResult {
        self.send_control_deadline(packet)
    }
}

impl RtxIo for NativeMediaIo {
    type Peer = SocketAddr;

    fn send_response(&mut self, peer: &Self::Peer, packet: &[u8]) -> SendResult {
        self.send_control_deadline_to(packet, *peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn binds_real_distinct_media_ports() {
        let io = NativeMediaIo::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let p = io.local_ports().unwrap();
        assert_ne!(p.data, 0);
        assert_ne!(p.control, 0);
        assert_ne!(p.data, p.control);
    }

    #[test]
    fn realtime_and_sync_use_separate_remote_ports() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let mut io = NativeMediaIo::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        io.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            data_rx.local_addr().unwrap().port(),
            ctrl_rx.local_addr().unwrap().port(),
        );
        assert_eq!(io.send_realtime(b"audio"), SendResult::Sent);
        assert_eq!(io.send_sync(b"sync"), SendResult::Sent);

        let mut buf = [0u8; 32];
        let (n, _) = data_rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"audio");
        let (n, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"sync");
    }

    #[test]
    fn missing_buffered_connection_is_fatal() {
        let mut io = NativeMediaIo::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(io.send_buffered(b"x"), StreamWrite::Fatal);
    }
}
