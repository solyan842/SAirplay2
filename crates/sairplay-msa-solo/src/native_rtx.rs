//! Source-faithful AP2 realtime RTP retransmit history/responder.
//! Pinned to music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128.

use crate::native_media::SendResult;

pub const RTX_RING_SLOTS: usize = 512;
pub const RTX_MAX_PACKET: usize = 12 + 352 * 6 + 8 + 16 + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RtxSlot {
    seq: u16,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct RtxRing {
    slots: Vec<Option<RtxSlot>>,
}

impl Default for RtxRing {
    fn default() -> Self {
        Self { slots: vec![None; RTX_RING_SLOTS] }
    }
}

impl RtxRing {
    pub fn store(&mut self, seq: u16, packet: &[u8]) -> bool {
        if packet.is_empty() || packet.len() > RTX_MAX_PACKET {
            return false;
        }
        self.slots[seq as usize % RTX_RING_SLOTS] =
            Some(RtxSlot { seq, bytes: packet.to_vec() });
        true
    }

    pub fn get(&self, seq: u16) -> Option<&[u8]> {
        match &self.slots[seq as usize % RTX_RING_SLOTS] {
            Some(slot) if slot.seq == seq => Some(&slot.bytes),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtxRequest {
    pub request_seq: u16,
    pub first_missing: u16,
    pub count: u16,
}

pub fn parse_request(buf: &[u8]) -> Option<RtxRequest> {
    if buf.len() < 8 || (buf[1] & 0x7f) != 0x55 {
        return None;
    }
    let request_seq = u16::from_be_bytes([buf[2], buf[3]]);
    let first_missing = u16::from_be_bytes([buf[4], buf[5]]);
    let raw_count = u16::from_be_bytes([buf[6], buf[7]]);
    let count = if raw_count == 0 {
        1
    } else {
        raw_count.min(RTX_RING_SLOTS as u16)
    };
    Some(RtxRequest { request_seq, first_missing, count })
}

pub fn build_response(request_seq: u16, original: &[u8]) -> Option<Vec<u8>> {
    if original.is_empty() || original.len() > RTX_MAX_PACKET {
        return None;
    }
    let mut out = Vec::with_capacity(4 + original.len());
    out.extend_from_slice(&[0x80, 0xd6]);
    out.extend_from_slice(&request_seq.to_be_bytes());
    out.extend_from_slice(original);
    Some(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RtxCounters {
    pub requested: u64,
    pub answered: u64,
    pub expired: u64,
    pub requested_over_1472: u64,
    pub max_requested_wire_len: u64,
}

pub trait RtxIo {
    type Peer;
    fn send_response(&mut self, peer: &Self::Peer, packet: &[u8]) -> SendResult;
}

pub fn serve_request<I: RtxIo>(
    ring: &RtxRing,
    counters: &mut RtxCounters,
    io: &mut I,
    peer: &I::Peer,
    datagram: &[u8],
) -> bool {
    let request = match parse_request(datagram) {
        Some(v) => v,
        None => return false,
    };
    counters.requested = counters.requested.saturating_add(request.count as u64);
    for k in 0..request.count {
        let seq = request.first_missing.wrapping_add(k);
        let original = match ring.get(seq) {
            Some(v) => v,
            None => {
                counters.expired = counters.expired.saturating_add(1);
                continue;
            }
        };
        let wire_len = original.len() as u64;
        if wire_len > 1472 {
            counters.requested_over_1472 =
                counters.requested_over_1472.saturating_add(1);
        }
        counters.max_requested_wire_len =
            counters.max_requested_wire_len.max(wire_len);
        let response = match build_response(request.request_seq, original) {
            Some(v) => v,
            None => {
                counters.expired = counters.expired.saturating_add(1);
                continue;
            }
        };
        if io.send_response(peer, &response) == SendResult::Sent {
            counters.answered = counters.answered.saturating_add(1);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_direct_seq_lookup_and_retires_by_overwrite() {
        let mut ring = RtxRing::default();
        assert!(ring.store(7, &[1, 2, 3]));
        assert_eq!(ring.get(7), Some(&[1, 2, 3][..]));
        let replacement = 7u16.wrapping_add(RTX_RING_SLOTS as u16);
        assert!(ring.store(replacement, &[9]));
        assert_eq!(ring.get(7), None);
        assert_eq!(ring.get(replacement), Some(&[9][..]));
    }

    #[test]
    fn request_zero_count_becomes_one_and_large_count_clamps() {
        let zero = [0x80, 0xd5, 0x12, 0x34, 0x00, 0x20, 0x00, 0x00];
        assert_eq!(parse_request(&zero).unwrap().count, 1);
        let huge = [0x80, 0xd5, 0x12, 0x34, 0x00, 0x20, 0xff, 0xff];
        assert_eq!(parse_request(&huge).unwrap().count, RTX_RING_SLOTS as u16);
    }

    #[test]
    fn response_wraps_original_packet_verbatim() {
        let out = build_response(0x1234, &[9, 8, 7]).unwrap();
        assert_eq!(out, vec![0x80, 0xd6, 0x12, 0x34, 9, 8, 7]);
    }

    struct FakeIo { result: SendResult, sent: Vec<Vec<u8>> }
    impl RtxIo for FakeIo {
        type Peer = ();
        fn send_response(&mut self, _peer: &(), packet: &[u8]) -> SendResult {
            self.sent.push(packet.to_vec());
            self.result
        }
    }

    #[test]
    fn responder_counts_requested_answered_and_expired() {
        let mut ring = RtxRing::default();
        ring.store(0x20, &[1, 2]);
        ring.store(0x21, &[3, 4]);
        let req = [0x80, 0xd5, 0x12, 0x34, 0x00, 0x20, 0x00, 0x03];
        let mut counters = RtxCounters::default();
        let mut io = FakeIo { result: SendResult::Sent, sent: Vec::new() };
        assert!(serve_request(&ring, &mut counters, &mut io, &(), &req));
        assert_eq!((counters.requested, counters.answered, counters.expired), (3, 2, 1));
        assert_eq!(io.sent[0], vec![0x80, 0xd6, 0x12, 0x34, 1, 2]);
    }
}
