use crate::{
    build_encrypted_realtime_packet, build_ntp_sync_packet, MediaTransport, MediaTransportError,
    NtpSyncPacketArgs, RtpState, FRAMES_PER_PACKET_44100,
};

#[derive(Debug)]
pub enum MediaSendError {
    Transport(MediaTransportError),
    Packet(crate::AudioPacketError),
}

impl From<MediaTransportError> for MediaSendError {
    fn from(value: MediaTransportError) -> Self { Self::Transport(value) }
}
impl From<crate::AudioPacketError> for MediaSendError {
    fn from(value: crate::AudioPacketError) -> Self { Self::Packet(value) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSendResult {
    pub sequence_sent: u16,
    pub timestamp_sent: u32,
    pub sync_sent: bool,
}

pub struct RealtimeMediaSender {
    transport: MediaTransport,
    state: RtpState,
    audio_key: [u8; 32],
    packets_since_sync: u16,
}

impl RealtimeMediaSender {
    pub fn new(transport: MediaTransport, state: RtpState, audio_key: [u8; 32]) -> Self {
        Self {
            transport,
            state,
            audio_key,
            packets_since_sync: 0,
        }
    }

    pub fn state(&self) -> RtpState {
        self.state
    }

    pub fn transport(&self) -> &MediaTransport {
        &self.transport
    }

    pub fn send_alac_payload(
        &mut self,
        alac_payload: &[u8],
        ntp_time: u64,
        lead_frames: u32,
    ) -> Result<MediaSendResult, MediaSendError> {
        let should_sync = self.state.first_packet || self.packets_since_sync >= 100;

        if should_sync {
            let play_position = self.state.timestamp.wrapping_sub(lead_frames);
            let sync = build_ntp_sync_packet(NtpSyncPacketArgs {
                first: self.state.first_packet,
                play_position,
                ntp_time,
                rtp_timestamp: self.state.timestamp,
            });
            self.transport.send_control(&sync)?;
        }

        let sequence_sent = self.state.sequence;
        let timestamp_sent = self.state.timestamp;
        let packet = build_encrypted_realtime_packet(&self.state, alac_payload, &self.audio_key)?;
        self.transport.send_data(&packet)?;

        self.state.advance(FRAMES_PER_PACKET_44100);

        if should_sync {
            self.packets_since_sync = 0;
        } else {
            self.packets_since_sync = self.packets_since_sync.wrapping_add(1);
        }

        Ok(MediaSendResult {
            sequence_sent,
            timestamp_sent,
            sync_sent: should_sync,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaTransport, StreamPorts};
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};
    use std::time::Duration;

    fn transport_to(data_rx: &UdpSocket, ctrl_rx: &UdpSocket) -> MediaTransport {
        let mut transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        transport.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            StreamPorts {
                data_port: data_rx.local_addr().unwrap().port(),
                control_port: ctrl_rx.local_addr().unwrap().port(),
            },
        );
        transport
    }

    #[test]
    fn first_send_emits_sync_then_encrypted_rtp_and_advances_352_frames() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(0x1234, 100_000, 0xAABBCCDD);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x44u8; 32]);

        let result = sender
            .send_alac_payload(b"fake-alac", 0x1122334455667788, 11_025)
            .unwrap();

        assert_eq!(
            result,
            MediaSendResult {
                sequence_sent: 0x1234,
                timestamp_sent: 100_000,
                sync_sent: true,
            }
        );

        let mut buf = [0u8; 2048];

        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 20);
        assert_eq!(&buf[..4], &[0x90, 0xD4, 0x00, 0x07]);
        assert_eq!(
            &buf[4..8],
            &100_000u32.wrapping_sub(11_025).to_be_bytes()
        );
        assert_eq!(&buf[16..20], &100_000u32.to_be_bytes());

        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert!(dn > 12 + 16 + 8);
        assert_eq!(&buf[..12], &state.header());

        let next = sender.state();
        assert_eq!(next.sequence, 0x1235);
        assert_eq!(next.timestamp, 100_352);
        assert!(!next.first_packet);
    }

    #[test]
    fn second_send_has_no_sync_and_marker_is_clear() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_millis(100))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 0, 0x01020304);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x22u8; 32]);

        sender.send_alac_payload(b"one", 1, 0).unwrap();
        let mut buf = [0u8; 2048];
        let _ = ctrl_rx.recv_from(&mut buf).unwrap();
        let _ = data_rx.recv_from(&mut buf).unwrap();

        let second = sender.send_alac_payload(b"two", 2, 0).unwrap();
        assert!(!second.sync_sent);

        assert!(ctrl_rx.recv_from(&mut buf).is_err());

        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert!(dn > 12);
        assert_eq!(buf[1], 0x60);
    }

    #[test]
    fn sync_repeats_after_100_intervening_packets() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(10, 0, 0x12345678);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x11u8; 32]);
        let mut buf = [0u8; 2048];

        sender.send_alac_payload(b"x", 1, 0).unwrap();
        let _ = ctrl_rx.recv_from(&mut buf).unwrap();
        let _ = data_rx.recv_from(&mut buf).unwrap();

        for _ in 0..100 {
            sender.send_alac_payload(b"x", 1, 0).unwrap();
            let _ = data_rx.recv_from(&mut buf).unwrap();
        }

        let repeated = sender.send_alac_payload(b"x", 1, 0).unwrap();
        assert!(repeated.sync_sent);
        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 20);
        assert_eq!(&buf[..2], &[0x80, 0xD4]);
    }
}
