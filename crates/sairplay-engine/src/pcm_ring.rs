use std::collections::VecDeque;

#[derive(Debug)]
pub struct PcmRing {
    data: VecDeque<i16>,
    capacity: usize,
}

impl PcmRing {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, samples: &[i16]) {
        for &sample in samples {
            if self.data.len() == self.capacity {
                self.data.pop_front();
            }
            self.data.push_back(sample);
        }
    }

    pub fn pop_or_silence(&mut self, count: usize) -> Vec<i16> {
        (0..count)
            .map(|_| self.data.pop_front().unwrap_or(0))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starvation_returns_exact_silence_packet() {
        let mut ring = PcmRing::new(4096);
        let packet = ring.pop_or_silence(704);
        assert_eq!(packet.len(), 704);
        assert!(packet.iter().all(|s| *s == 0));
    }

    #[test]
    fn source_gap_does_not_end_ring() {
        let mut ring = PcmRing::new(16);
        ring.push(&[1, 2, 3, 4]);
        assert_eq!(ring.pop_or_silence(8), vec![1,2,3,4,0,0,0,0]);
    }
}
