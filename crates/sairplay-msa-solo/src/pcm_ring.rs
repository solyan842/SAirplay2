//! Persistent single-owner PCM ring semantics ported from MSA ap2_session.c.

const MIN_BYTES: usize = 1 << 20;
const SECONDS: usize = 4;

#[derive(Debug)]
pub struct PcmRing {
    data: Vec<u8>,
    rd: usize,
    wr: usize,
    fill: usize,
    eof: bool,
}

impl PcmRing {
    pub fn for_byte_rate(byte_rate: usize) -> Self {
        let cap = (byte_rate * SECONDS).max(MIN_BYTES);
        Self { data: vec![0; cap], rd: 0, wr: 0, fill: 0, eof: false }
    }
    pub fn capacity(&self) -> usize { self.data.len() }
    pub fn fill(&self) -> usize { self.fill }
    pub fn eof(&self) -> bool { self.eof }
    pub fn mark_eof(&mut self) { self.eof = true; }
    pub fn reset(&mut self) {
        self.rd = 0; self.wr = 0; self.fill = 0; self.eof = false;
    }
    pub fn push(&mut self, input: &[u8]) -> usize {
        let n = input.len().min(self.capacity() - self.fill);
        let first = n.min(self.capacity() - self.wr);
        self.data[self.wr..self.wr + first].copy_from_slice(&input[..first]);
        let second = n - first;
        if second > 0 { self.data[..second].copy_from_slice(&input[first..first + second]); }
        self.wr = (self.wr + n) % self.capacity();
        self.fill += n;
        n
    }
    pub fn pop(&mut self, output: &mut [u8]) -> usize {
        let n = output.len().min(self.fill);
        let first = n.min(self.capacity() - self.rd);
        output[..first].copy_from_slice(&self.data[self.rd..self.rd + first]);
        let second = n - first;
        if second > 0 { output[first..first + second].copy_from_slice(&self.data[..second]); }
        self.rd = (self.rd + n) % self.capacity();
        self.fill -= n;
        n
    }
    pub fn discard(&mut self, want: usize) -> usize {
        let n = want.min(self.fill);
        self.rd = (self.rd + n) % self.capacity();
        self.fill -= n;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ring_wrap_reset_and_discard_match_msa_shape() {
        let mut ring = PcmRing::for_byte_rate(16);
        assert_eq!(ring.capacity(), MIN_BYTES);
        assert_eq!(ring.push(&[1,2,3,4]), 4);
        let mut out=[0u8;2];
        assert_eq!(ring.pop(&mut out),2);
        assert_eq!(out,[1,2]);
        assert_eq!(ring.discard(1),1);
        assert_eq!(ring.fill(),1);
        ring.mark_eof();
        assert!(ring.eof());
        ring.reset();
        assert_eq!(ring.fill(),0);
        assert!(!ring.eof());
    }
}
