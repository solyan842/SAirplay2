//! MSA-clone persistent session core for SAirplay 2.0.
//!
//! This mirrors the ownership/state semantics of pinned Music Assistant
//! `ap2_session.c`: one persistent session, one bounded PCM ring, explicit
//! IDLE/PLAYING/STANDBY/ENDED state, START epochs, audio-present readiness and
//! FLUSH resetting old content. Transport operations are deliberately outside
//! this core and will be attached through an MSA-style adapter.

use std::collections::VecDeque;

pub const MSA_SESSION_RING_SECONDS: usize = 4;
pub const MSA_SESSION_RING_MIN_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsaSessionState {
    Idle,
    Playing,
    Standby,
    Ended,
}

#[derive(Debug)]
pub struct MsaSessionCore {
    state: MsaSessionState,
    epoch: u64,
    byte_rate: usize,
    ready_bytes: usize,
    capacity_bytes: usize,
    ring: VecDeque<u8>,
    audio_seen: bool,
}

impl MsaSessionCore {
    pub fn new(byte_rate: usize, ready_bytes: usize) -> Result<Self, String> {
        if byte_rate == 0 {
            return Err("MSA session byte_rate must be non-zero".into());
        }
        if ready_bytes == 0 {
            return Err("MSA session ready_bytes must be non-zero".into());
        }

        let capacity_bytes = byte_rate
            .saturating_mul(MSA_SESSION_RING_SECONDS)
            .max(MSA_SESSION_RING_MIN_BYTES);
        if ready_bytes > capacity_bytes {
            return Err("MSA session ready buffer exceeds ring capacity".into());
        }

        Ok(Self {
            state: MsaSessionState::Idle,
            epoch: 0,
            byte_rate,
            ready_bytes,
            capacity_bytes,
            ring: VecDeque::with_capacity(capacity_bytes),
            audio_seen: false,
        })
    }

    pub fn state(&self) -> MsaSessionState {
        self.state
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn byte_rate(&self) -> usize {
        self.byte_rate
    }

    pub fn ready_bytes(&self) -> usize {
        self.ready_bytes
    }

    pub fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }

    pub fn buffered_bytes(&self) -> usize {
        self.ring.len()
    }

    pub fn audio_present(&self) -> bool {
        self.audio_seen
    }

    /// Persistent input reader equivalent: buffer PCM irrespective of PLAYING.
    /// Pre-START audio is allowed to prime the ring exactly as MSA does.
    pub fn push_input(&mut self, bytes: &[u8]) -> usize {
        if self.state == MsaSessionState::Ended || bytes.is_empty() {
            return 0;
        }

        let room = self.capacity_bytes.saturating_sub(self.ring.len());
        let count = bytes.len().min(room);
        self.ring.extend(bytes[..count].iter().copied());

        if !self.audio_seen && self.ring.len() >= self.ready_bytes {
            self.audio_seen = true;
        }
        count
    }

    /// MSA START state transition. Transport commit/quiesce is performed by
    /// the higher-level session adapter; this core owns only source state.
    pub fn start_committed(&mut self) -> Result<u64, String> {
        if self.state == MsaSessionState::Ended {
            return Err("cannot START an ended MSA session".into());
        }
        self.epoch = self.epoch.saturating_add(1);
        self.state = MsaSessionState::Playing;
        Ok(self.epoch)
    }

    /// Read only while PLAYING. Temporary lack of a full packet is starvation,
    /// never an inferred pause/EOF and never a state transition.
    pub fn read_playing(&mut self, want: usize) -> Result<Option<Vec<u8>>, String> {
        if self.state == MsaSessionState::Ended {
            return Err("MSA session ended".into());
        }
        if self.state != MsaSessionState::Playing {
            return Ok(None);
        }
        if want == 0 {
            return Ok(Some(Vec::new()));
        }
        if self.ring.len() < want {
            return Ok(None);
        }

        let mut out = Vec::with_capacity(want);
        for _ in 0..want {
            out.push(self.ring.pop_front().expect("ring length checked"));
        }
        Ok(Some(out))
    }

    /// Exact source-side FLUSH boundary: old buffered content is discarded,
    /// audio-present is re-armed and the session returns to IDLE.
    pub fn flush_committed(&mut self) -> Result<(), String> {
        if self.state == MsaSessionState::Ended {
            return Err("cannot FLUSH an ended MSA session".into());
        }
        self.ring.clear();
        self.audio_seen = false;
        self.state = MsaSessionState::Idle;
        Ok(())
    }

    pub fn standby_committed(&mut self) -> Result<(), String> {
        if self.state == MsaSessionState::Ended {
            return Err("cannot STANDBY an ended MSA session".into());
        }
        self.state = MsaSessionState::Standby;
        Ok(())
    }

    pub fn end(&mut self) {
        self.state = MsaSessionState::Ended;
        self.ring.clear();
        self.audio_seen = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> MsaSessionCore {
        MsaSessionCore::new(176_400, 1_408).unwrap()
    }

    #[test]
    fn starts_idle_and_primes_before_start() {
        let mut s = session();
        assert_eq!(s.state(), MsaSessionState::Idle);
        assert_eq!(s.epoch(), 0);

        assert_eq!(s.push_input(&vec![1u8; 1_407]), 1_407);
        assert!(!s.audio_present());
        assert_eq!(s.push_input(&[2u8]), 1);
        assert!(s.audio_present());

        assert_eq!(s.start_committed().unwrap(), 1);
        assert_eq!(s.state(), MsaSessionState::Playing);
    }

    #[test]
    fn zero_pcm_is_audio_not_state() {
        let mut s = session();
        s.push_input(&vec![0u8; 1_408]);
        assert!(s.audio_present());
        s.start_committed().unwrap();
        let packet = s.read_playing(1_408).unwrap().unwrap();
        assert!(packet.iter().all(|byte| *byte == 0));
        assert_eq!(s.state(), MsaSessionState::Playing);
    }

    #[test]
    fn idle_or_standby_never_consumes_source() {
        let mut s = session();
        s.push_input(&vec![9u8; 1_408]);
        assert!(s.read_playing(1_408).unwrap().is_none());
        assert_eq!(s.buffered_bytes(), 1_408);

        s.start_committed().unwrap();
        s.standby_committed().unwrap();
        assert!(s.read_playing(1_408).unwrap().is_none());
        assert_eq!(s.buffered_bytes(), 1_408);
    }

    #[test]
    fn flush_discards_old_content_and_rearms_audio_present() {
        let mut s = session();
        s.push_input(&vec![7u8; 2_816]);
        s.start_committed().unwrap();
        assert!(s.audio_present());

        s.flush_committed().unwrap();
        assert_eq!(s.state(), MsaSessionState::Idle);
        assert_eq!(s.buffered_bytes(), 0);
        assert!(!s.audio_present());

        s.push_input(&vec![3u8; 1_408]);
        assert!(s.audio_present());
        assert_eq!(s.start_committed().unwrap(), 2);
    }

    #[test]
    fn end_is_terminal() {
        let mut s = session();
        s.push_input(&vec![1u8; 1_408]);
        s.end();
        assert_eq!(s.state(), MsaSessionState::Ended);
        assert_eq!(s.buffered_bytes(), 0);
        assert_eq!(s.push_input(&[1, 2, 3]), 0);
        assert!(s.start_committed().is_err());
        assert!(s.flush_committed().is_err());
        assert!(s.standby_committed().is_err());
        assert!(s.read_playing(1).is_err());
    }
}
