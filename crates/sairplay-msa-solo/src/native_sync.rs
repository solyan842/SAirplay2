//! Source-faithful native AP2 realtime sync packets.
//! Pinned to music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128.

use crate::native_media::{MediaHealth, SendResult};
use crate::time_domain::SourceNtp;

pub const PTP_FRAME_1_OFFSET: u32 = 11_035;
pub const PTP_FRAME_2_OFFSET: u32 = 77_175;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncCounters {
    pub sent: u64,
    pub dropped: u64,
}

pub trait SyncIo {
    fn send_sync(&mut self, packet: &[u8]) -> SendResult;
}

pub fn execute_sync<I: SyncIo>(
    io: &mut I,
    health: &mut MediaHealth,
    counters: &mut SyncCounters,
    packet: &[u8],
) -> SendResult {
    let result = io.send_sync(packet);
    match result {
        SendResult::Sent => counters.sent = counters.sent.saturating_add(1),
        SendResult::Dropped => counters.dropped = counters.dropped.saturating_add(1),
        SendResult::Fatal => {
            counters.dropped = counters.dropped.saturating_add(1);
            health.healthy = false;
        }
    }
    result
}

pub fn build_ntp_sync(
    first: bool,
    rtp_timestamp: u32,
    lead_ms: u64,
    sample_rate: u32,
    ntp: SourceNtp,
) -> [u8; 20] {
    let mut pkt = [0u8; 20];
    pkt[0] = if first { 0x90 } else { 0x80 };
    pkt[1] = 0xd4;
    pkt[2] = 0x00;
    pkt[3] = 0x07;

    let latency_frames = crate::native_timeline::frames_for_ms(lead_ms, sample_rate) as u32;
    let rendering = if rtp_timestamp >= latency_frames {
        rtp_timestamp - latency_frames
    } else {
        0
    };
    pkt[4..8].copy_from_slice(&rendering.to_be_bytes());
    let raw = ntp.raw();
    pkt[8..12].copy_from_slice(&((raw >> 32) as u32).to_be_bytes());
    pkt[12..16].copy_from_slice(&(raw as u32).to_be_bytes());
    pkt[16..20].copy_from_slice(&rtp_timestamp.to_be_bytes());
    pkt
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PtpAnchor {
    pub valid: bool,
    pub wall0_ns: u64,
    pub pos0: u32,
}

impl PtpAnchor {
    pub fn freeze_if_needed(
        &mut self,
        master_now_ns: u64,
        local_ptp_now_ns: u64,
        start_ntp: SourceNtp,
        lead_ms: u64,
        sample_rate: u32,
        rtp_offset: u32,
        current_rtp: u32,
    ) {
        if self.valid {
            return;
        }
        if start_ntp != SourceNtp::ZERO {
            let unix_ns = start_ntp.to_unix_ns();
            let master_shift = master_now_ns as i128 - local_ptp_now_ns as i128;
            let shifted = (unix_ns as i128 + master_shift) as u64;
            self.wall0_ns = shifted.wrapping_sub(lead_ms.saturating_mul(1_000_000));
            self.pos0 = (start_ntp.to_frames(sample_rate) as u32)
                .wrapping_add(rtp_offset);
        } else {
            self.wall0_ns = master_now_ns;
            self.pos0 = current_rtp;
        }
        self.valid = true;
    }
}

pub fn build_ptp_sync(
    first: bool,
    current_rtp: u32,
    lead_ms: u64,
    sample_rate: u32,
    master_now_ns: u64,
    master_clock_id: u64,
    anchor: PtpAnchor,
) -> [u8; 28] {
    assert!(anchor.valid);
    let wall_delta_ns = if master_now_ns >= anchor.wall0_ns {
        (master_now_ns - anchor.wall0_ns) as i128
    } else {
        -((anchor.wall0_ns - master_now_ns) as i128)
    };
    let elapsed_ns = wall_delta_ns - lead_ms as i128 * 1_000_000i128;
    let frame_delta = elapsed_ns * sample_rate as i128 / 1_000_000_000i128;
    let play_pos = anchor.pos0.wrapping_add(frame_delta as u32);
    let frame_1 = play_pos.wrapping_add(PTP_FRAME_1_OFFSET);
    let frame_2 = frame_1.wrapping_add(PTP_FRAME_2_OFFSET);

    let mut pkt = [0u8; 28];
    pkt[0] = if first { 0x90 } else { 0x80 };
    pkt[1] = 0xd7;
    pkt[2] = 0x00;
    pkt[3] = 0x06;
    pkt[4..8].copy_from_slice(&frame_1.to_be_bytes());
    pkt[8..16].copy_from_slice(&master_now_ns.to_be_bytes());
    pkt[16..20].copy_from_slice(&frame_2.to_be_bytes());
    pkt[20..28].copy_from_slice(&master_clock_id.to_be_bytes());

    let _send_ahead = current_rtp.wrapping_sub(play_pos) as i32;
    pkt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_sync_matches_msa_wire_layout() {
        let ntp = SourceNtp::from_raw((123u64 << 32) | 0x1122_3344);
        let pkt = build_ntp_sync(true, 100_000, 250, 48_000, ntp);
        assert_eq!(&pkt[0..4], &[0x90, 0xd4, 0x00, 0x07]);
        assert_eq!(u32::from_be_bytes(pkt[4..8].try_into().unwrap()), 88_000);
        assert_eq!(u32::from_be_bytes(pkt[8..12].try_into().unwrap()), 123);
        assert_eq!(u32::from_be_bytes(pkt[12..16].try_into().unwrap()), 0x1122_3344);
        assert_eq!(u32::from_be_bytes(pkt[16..20].try_into().unwrap()), 100_000);
    }

    #[test]
    fn ptp_anchor_freezes_once_and_packet_matches_msa_shape() {
        let start_ntp = SourceNtp::from_unix_ms(10_000);
        let mut anchor = PtpAnchor::default();
        anchor.freeze_if_needed(
            20_000_000_000,
            19_000_000_000,
            start_ntp,
            250,
            48_000,
            1_000,
            7,
        );
        let frozen = anchor;
        anchor.freeze_if_needed(99, 1, start_ntp, 999, 44_100, 9, 9);
        assert_eq!(anchor, frozen);

        let pkt = build_ptp_sync(
            true,
            frozen.pos0.wrapping_add(12_000),
            250,
            48_000,
            frozen.wall0_ns + 250_000_000,
            0x0102_0304_0506_0708,
            frozen,
        );
        assert_eq!(&pkt[0..4], &[0x90, 0xd7, 0x00, 0x06]);
        assert_eq!(
            u32::from_be_bytes(pkt[4..8].try_into().unwrap()),
            frozen.pos0.wrapping_add(PTP_FRAME_1_OFFSET)
        );
        assert_eq!(
            u32::from_be_bytes(pkt[16..20].try_into().unwrap()),
            frozen.pos0.wrapping_add(PTP_FRAME_1_OFFSET).wrapping_add(PTP_FRAME_2_OFFSET)
        );
        assert_eq!(u64::from_be_bytes(pkt[20..28].try_into().unwrap()), 0x0102_0304_0506_0708);
    }

    struct FakeSyncIo(SendResult);
    impl SyncIo for FakeSyncIo {
        fn send_sync(&mut self, _packet: &[u8]) -> SendResult { self.0 }
    }

    #[test]
    fn fatal_sync_marks_media_unhealthy() {
        let mut io = FakeSyncIo(SendResult::Fatal);
        let mut health = MediaHealth::default();
        let mut counters = SyncCounters::default();
        assert_eq!(execute_sync(&mut io, &mut health, &mut counters, &[1]), SendResult::Fatal);
        assert!(!health.healthy);
        assert_eq!((counters.sent, counters.dropped), (0, 1));
    }
}
