//! Windows PCM source ownership for the SAirplay 2.0 MSA clone.
//!
//! This is the first runtime integration boundary. WASAPI is a producer only:
//! it may push captured PCM, but it does not own playback state, pause/idle
//! inference or source consumption. Those belong exclusively to MsaSessionCore.

use crate::{MsaSessionCore, MsaSessionState};

pub const WINDOWS_PCM_PACKET_BYTES_16_441_STEREO: usize = 352 * 2 * 2;

#[derive(Debug)]
pub struct MsaWindowsSource {
    core: MsaSessionCore,
}

impl MsaWindowsSource {
    pub fn new(
        byte_rate: usize,
        ready_bytes: usize,
    ) -> Result<Self, String> {
        Ok(Self {
            core: MsaSessionCore::new(byte_rate, ready_bytes)?,
        })
    }

    pub fn core(&self) -> &MsaSessionCore {
        &self.core
    }

    pub fn core_mut(&mut self) -> &mut MsaSessionCore {
        &mut self.core
    }

    /// Called by the Windows capture producer. PCM amplitude is intentionally
    /// ignored: zero PCM is still real source data and may satisfy readiness.
    pub fn push_capture_pcm(&mut self, pcm: &[u8]) -> usize {
        self.core.push_input(pcm)
    }

    /// Sender-facing packet read. Only PLAYING may consume the source.
    pub fn next_packet(&mut self, packet_bytes: usize) -> Result<Option<Vec<u8>>, String> {
        self.core.read_playing(packet_bytes)
    }

    pub fn state(&self) -> MsaSessionState {
        self.core.state()
    }

    pub fn audio_present(&self) -> bool {
        self.core.audio_present()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.core.buffered_bytes()
    }

    pub fn input_room_bytes(&self) -> usize {
        self.core
            .capacity_bytes()
            .saturating_sub(self.core.buffered_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> MsaWindowsSource {
        MsaWindowsSource::new(
            44_100 * 2 * 2,
            WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
        )
        .unwrap()
    }

    #[test]
    fn wasapi_pcm_primes_session_before_start_without_consuming() {
        let mut source = source();
        let packet = vec![1u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO];

        assert_eq!(source.push_capture_pcm(&packet), packet.len());
        assert!(source.audio_present());
        assert_eq!(source.state(), MsaSessionState::Idle);
        assert!(source.next_packet(packet.len()).unwrap().is_none());
        assert_eq!(source.buffered_bytes(), packet.len());
    }

    #[test]
    fn digital_zero_pcm_counts_as_real_audio() {
        let mut source = source();
        let silence = vec![0u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO];

        source.push_capture_pcm(&silence);
        assert!(source.audio_present());
        assert_eq!(source.state(), MsaSessionState::Idle);
    }

    #[test]
    fn only_explicit_start_allows_sender_to_consume() {
        let mut source = source();
        let packet = vec![9u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO];

        source.push_capture_pcm(&packet);
        source.core_mut().start_committed().unwrap();

        let out = source.next_packet(packet.len()).unwrap().unwrap();
        assert_eq!(out, packet);
        assert_eq!(source.state(), MsaSessionState::Playing);
    }

    #[test]
    fn empty_capture_poll_does_not_change_state() {
        let mut source = source();
        source.core_mut().start_committed().unwrap();

        for _ in 0..10_000 {
            assert_eq!(source.push_capture_pcm(&[]), 0);
        }

        assert_eq!(source.state(), MsaSessionState::Playing);
    }

    #[test]
    fn flush_is_the_only_boundary_that_discards_old_windows_pcm() {
        let mut source = source();
        let packet = vec![5u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO];

        source.push_capture_pcm(&packet);
        source.core_mut().start_committed().unwrap();
        source.core_mut().flush_committed().unwrap();

        assert_eq!(source.state(), MsaSessionState::Idle);
        assert_eq!(source.buffered_bytes(), 0);
        assert!(!source.audio_present());
    }

    #[test]
    fn standby_does_not_consume_or_discard_buffered_pcm() {
        let mut source = source();
        let packet = vec![3u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO];

        source.push_capture_pcm(&packet);
        source.core_mut().start_committed().unwrap();
        source.core_mut().standby_committed().unwrap();

        assert_eq!(source.buffered_bytes(), packet.len());
        assert!(source.next_packet(packet.len()).unwrap().is_none());
        assert_eq!(source.state(), MsaSessionState::Standby);
    }
}
