pub const RTP_PAYLOAD_TYPE_REALTIME: u8 = 96;
pub const RTP_MARKER_BIT: u8 = 0x80;
pub const FRAMES_PER_PACKET_44100: u32 = 352;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpState {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub first_packet: bool,
}

impl RtpState {
    pub fn new(sequence: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            sequence,
            timestamp,
            ssrc,
            first_packet: true,
        }
    }

    pub fn header(&self) -> [u8; 12] {
        build_rtp_header(
            self.sequence,
            self.timestamp,
            self.ssrc,
            self.first_packet,
        )
    }

    pub fn advance(&mut self, frames: u32) {
        self.advance_with_marker_clear(frames, true);
    }

    pub fn advance_with_marker_clear(&mut self, frames: u32, clear_first: bool) {
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(frames);
        if clear_first {
            self.first_packet = false;
        }
    }
}

pub fn build_rtp_header(
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    first_packet: bool,
) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0] = 0x80; // RTP v2
    header[1] = RTP_PAYLOAD_TYPE_REALTIME
        | if first_packet { RTP_MARKER_BIT } else { 0 };
    header[2..4].copy_from_slice(&sequence.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    header
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpSyncPacketArgs {
    pub first: bool,
    /// Current play position field written at bytes 4..8.
    /// Current airplay-cli uses rtp_timestamp - latency frames here.
    pub play_position: u32,
    /// NTP fixed-point 32.32 timestamp.
    pub ntp_time: u64,
    /// Current RTP timestamp at bytes 16..20.
    pub rtp_timestamp: u32,
}

pub fn build_ntp_sync_packet(args: NtpSyncPacketArgs) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0] = if args.first { 0x90 } else { 0x80 };
    packet[1] = 0xD4;
    packet[2] = 0x00;
    packet[3] = 0x07;
    packet[4..8].copy_from_slice(&args.play_position.to_be_bytes());
    let ntp_seconds = (args.ntp_time >> 32) as u32;
    let ntp_fraction = args.ntp_time as u32;
    packet[8..12].copy_from_slice(&ntp_seconds.to_be_bytes());
    packet[12..16].copy_from_slice(&ntp_fraction.to_be_bytes());
    packet[16..20].copy_from_slice(&args.rtp_timestamp.to_be_bytes());
    packet
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpSyncPacketArgs {
    pub first: bool,
    pub frame_1: u32,
    pub wall_time_ns: u64,
    pub frame_2: u32,
    pub clock_id: u64,
}

pub fn build_ptp_sync_packet(args: PtpSyncPacketArgs) -> [u8; 28] {
    let mut packet = [0u8; 28];
    packet[0] = if args.first { 0x90 } else { 0x80 };
    packet[1] = 0xD7;
    packet[2] = 0x00;
    packet[3] = 0x06;
    packet[4..8].copy_from_slice(&args.frame_1.to_be_bytes());
    packet[8..16].copy_from_slice(&args.wall_time_ns.to_be_bytes());
    packet[16..20].copy_from_slice(&args.frame_2.to_be_bytes());
    packet[20..28].copy_from_slice(&args.clock_id.to_be_bytes());
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_rtp_packet_sets_marker_and_payload_type_96() {
        let header = build_rtp_header(0x1234, 0x11223344, 0x55667788, true);
        assert_eq!(header[0], 0x80);
        assert_eq!(header[1], 0xE0);
        assert_eq!(&header[2..4], &0x1234u16.to_be_bytes());
        assert_eq!(&header[4..8], &0x11223344u32.to_be_bytes());
        assert_eq!(&header[8..12], &0x55667788u32.to_be_bytes());
    }

    #[test]
    fn subsequent_rtp_packet_clears_marker_but_keeps_type_96() {
        let header = build_rtp_header(7, 352, 0xAABBCCDD, false);
        assert_eq!(header[1], 0x60);
    }

    #[test]
    fn rtp_state_advances_exactly_one_sequence_and_352_frames() {
        let mut state = RtpState::new(65535, 0xFFFF_FF00, 0x12345678);
        assert!(state.first_packet);

        state.advance(FRAMES_PER_PACKET_44100);

        assert_eq!(state.sequence, 0);
        assert_eq!(state.timestamp, 0x0000_0060);
        assert!(!state.first_packet);
    }

    #[test]
    fn ntp_sync_first_packet_matches_source_layout() {
        let packet = build_ntp_sync_packet(NtpSyncPacketArgs {
            first: true,
            play_position: 0x01020304,
            ntp_time: 0x1122334455667788,
            rtp_timestamp: 0x99AABBCC,
        });

        assert_eq!(&packet[0..4], &[0x90, 0xD4, 0x00, 0x07]);
        assert_eq!(&packet[4..8], &0x01020304u32.to_be_bytes());
        assert_eq!(&packet[8..12], &0x11223344u32.to_be_bytes());
        assert_eq!(&packet[12..16], &0x55667788u32.to_be_bytes());
        assert_eq!(&packet[16..20], &0x99AABBCCu32.to_be_bytes());
    }

    #[test]
    fn ptp_sync_matches_source_layout() {
        let packet = build_ptp_sync_packet(PtpSyncPacketArgs {
            first: true,
            frame_1: 0x01020304,
            wall_time_ns: 0x1122334455667788,
            frame_2: 0x99AABBCC,
            clock_id: 0xA1B2C3D4E5F60708,
        });
        assert_eq!(&packet[0..4], &[0x90, 0xD7, 0x00, 0x06]);
        assert_eq!(&packet[4..8], &0x01020304u32.to_be_bytes());
        assert_eq!(&packet[8..16], &0x1122334455667788u64.to_be_bytes());
        assert_eq!(&packet[16..20], &0x99AABBCCu32.to_be_bytes());
        assert_eq!(&packet[20..28], &0xA1B2C3D4E5F60708u64.to_be_bytes());
    }

    #[test]
    fn ntp_sync_subsequent_packet_uses_0x80_d4() {
        let packet = build_ntp_sync_packet(NtpSyncPacketArgs {
            first: false,
            play_position: 1,
            ntp_time: 2,
            rtp_timestamp: 3,
        });
        assert_eq!(&packet[0..2], &[0x80, 0xD4]);
    }
}
