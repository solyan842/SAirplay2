use std::collections::VecDeque;

pub const PCM352_PACKET_BYTES: usize = 352 * 2 * 2;

#[derive(Debug, Default)]
pub struct Pcm352Chunker {
    pending: VecDeque<u8>,
}

impl Pcm352Chunker {
    pub fn new() -> Self { Self::default() }

    pub fn pending_bytes(&self) -> usize { self.pending.len() }

    pub fn has_packet(&self) -> bool { self.pending.len() >= PCM352_PACKET_BYTES }

    pub fn clear(&mut self) { self.pending.clear(); }

    pub fn push(&mut self, pcm_le_stereo_16: &[u8]) {
        self.pending.extend(pcm_le_stereo_16.iter().copied());
    }

    pub fn pop_packet_padded_silence(&mut self) -> [u8; PCM352_PACKET_BYTES] {
        let mut out = [0u8; PCM352_PACKET_BYTES];
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
    ) -> Option<[u8; PCM352_PACKET_BYTES]> {
        let pad_frames = pad_frames.min(352) as usize;
        let pad_bytes = pad_frames * 4;
        let want = PCM352_PACKET_BYTES - pad_bytes;
        if self.pending.len() < want {
            return None;
        }

        let mut out = [0u8; PCM352_PACKET_BYTES];
        for byte in &mut out[pad_bytes..] {
            *byte = self.pending.pop_front().expect("length checked");
        }
        Some(out)
    }

    pub fn pop_packet(&mut self) -> Option<[u8; PCM352_PACKET_BYTES]> {
        if self.pending.len() < PCM352_PACKET_BYTES {
            return None;
        }
        let mut out = [0u8; PCM352_PACKET_BYTES];
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
        c.push(&[1,2,3,4,5]);
        c.clear();
        assert_eq!(c.pending_bytes(), 0);
    }
}
