use std::collections::VecDeque;

pub const PCM352_PACKET_BYTES: usize = 352 * 2 * 2;

#[derive(Debug)]
pub struct Pcm352Chunker {
    pending: VecDeque<u8>,
    bytes_per_frame: usize,
    packet_bytes: usize,
    #[test]
    fn supports_source_24bit_s32le_carrier_packets() {
        let mut c = Pcm352Chunker::new_with_bytes_per_frame(8);
        assert_eq!(c.packet_bytes(), 352 * 8);
        c.push(&vec![0x44; 352 * 8]);
        let out = c.pop_packet().unwrap();
        assert_eq!(out.len(), 352 * 8);
        assert!(out.iter().all(|b| *b == 0x44));
    }

}

impl Default for Pcm352Chunker {
    fn default() -> Self {
        Self::new_with_bytes_per_frame(4)
    }
}

impl Pcm352Chunker {
    pub fn new() -> Self { Self::default() }

    pub fn new_with_bytes_per_frame(bytes_per_frame: usize) -> Self {
        assert!(bytes_per_frame > 0);
        Self {
            pending: VecDeque::new(),
            bytes_per_frame,
            packet_bytes: 352 * bytes_per_frame,
        }
    }

    pub fn bytes_per_frame(&self) -> usize { self.bytes_per_frame }
    pub fn packet_bytes(&self) -> usize { self.packet_bytes }
    pub fn pending_bytes(&self) -> usize { self.pending.len() }

    pub fn pending_nonzero_bytes(&self) -> usize {
        self.pending.iter().filter(|byte| **byte != 0).count()
    }

    pub fn has_packet(&self) -> bool { self.pending.len() >= self.packet_bytes }

    pub fn clear(&mut self) { self.pending.clear(); }

    pub fn push(&mut self, pcm_le_stereo_16: &[u8]) {
        self.pending.extend(pcm_le_stereo_16.iter().copied());
    }

    pub fn pop_packet_padded_silence(&mut self) -> Vec<u8> {
        let mut out = vec![0u8; self.packet_bytes];
        for byte in &mut out {
            match self.pending.pop_front() {
                Some(value) => *byte = value,
                None => break,
            }
        }
        out
    }

    pub fn pop_packet_with_silence_prefix(
        &mut self,
        pad_frames: u32,
    ) -> Option<Vec<u8>> {
        let pad_frames = pad_frames.min(352) as usize;
        let pad_bytes = pad_frames * self.bytes_per_frame;
        let want = self.packet_bytes - pad_bytes;
        if self.pending.len() < want {
            return None;
        }

        let mut out = vec![0u8; self.packet_bytes];
        for byte in &mut out[pad_bytes..] {
            *byte = self.pending.pop_front().expect("length checked");
        }
        Some(out)
    }

    pub fn pop_packet(&mut self) -> Option<Vec<u8>> {
        if self.pending.len() < self.packet_bytes {
            return None;
        }
        let mut out = vec![0u8; self.packet_bytes];
        for byte in &mut out {
            *byte = self.pending.pop_front().unwrap();
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_exactly_one_352_frame_packet() {
        let mut c = Pcm352Chunker::new();
        let input: Vec<u8> = (0..PCM352_PACKET_BYTES).map(|i| (i & 0xff) as u8).collect();
        c.push(&input);
        let out = c.pop_packet().unwrap();
        assert_eq!(out.as_slice(), input.as_slice());
        assert_eq!(c.pending_bytes(), 0);
        assert!(c.pop_packet().is_none());
    }

    #[test]
    fn preserves_partial_input_until_packet_is_complete() {
        let mut c = Pcm352Chunker::new();
        c.push(&vec![0x11; 1000]);
        assert!(c.pop_packet().is_none());
        assert_eq!(c.pending_bytes(), 1000);
        c.push(&vec![0x22; PCM352_PACKET_BYTES - 1000]);
        let out = c.pop_packet().unwrap();
        assert!(out[..1000].iter().all(|b| *b == 0x11));
        assert!(out[1000..].iter().all(|b| *b == 0x22));
        assert_eq!(c.pending_bytes(), 0);
    }

    #[test]
    fn splice_pad_is_sample_exact_and_preserves_unused_real_pcm() {
        let mut c = Pcm352Chunker::new();
        c.push(&vec![0x66; PCM352_PACKET_BYTES]);

        let out = c.pop_packet_with_silence_prefix(100).unwrap();
        assert!(out[..400].iter().all(|b| *b == 0));
        assert!(out[400..].iter().all(|b| *b == 0x66));
        assert_eq!(c.pending_bytes(), 400);
    }

    #[test]
    fn starvation_consumes_partial_tail_then_pads_silence() {
        let mut c = Pcm352Chunker::new();
        c.push(&vec![0x55; 1000]);
        let out = c.pop_packet_padded_silence();
        assert!(out[..1000].iter().all(|b| *b == 0x55));
        assert!(out[1000..].iter().all(|b| *b == 0));
        assert_eq!(c.pending_bytes(), 0);
    }

    #[test]
    fn handles_multiple_packets_without_dropping_tail() {
        let mut c = Pcm352Chunker::new();
        c.push(&vec![0x33; PCM352_PACKET_BYTES * 2 + 17]);
        assert!(c.pop_packet().is_some());
        assert!(c.pop_packet().is_some());
        assert_eq!(c.pending_bytes(), 17);
        assert!(c.pop_packet().is_none());
    }

    #[test]
    fn clear_discards_only_buffered_capture_bytes() {
        let mut c = Pcm352Chunker::new();
        c.push(&[1,2,0,4,0]);
        assert_eq!(c.pending_nonzero_bytes(), 3);
        c.clear();
        assert_eq!(c.pending_bytes(), 0);
        assert_eq!(c.pending_nonzero_bytes(), 0);
    }
}
