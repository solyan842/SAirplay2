//! Shared Windows PCM ownership seam for MSA receiver sessions.
//!
//! Phase 2A deliberately centralizes only local PCM state. It does not decide
//! AirPlay lifecycle, START/FLUSH semantics, pacing, ALAC, RTP, PTP or route
//! policy. Those remain owned by the MSA receiver transport.

use crate::{Ap2AudioFormat, Pcm352Chunker};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

#[derive(Clone)]
pub struct WindowsPcmHub {
    ring: Arc<Mutex<Pcm352Chunker>>,
    ring_capacity: usize,
    flush_generation: Arc<AtomicU64>,
    flush_ack_generation: Arc<AtomicU64>,
    audio_ready: Arc<AtomicBool>,
    source_present: Arc<AtomicBool>,
    capture_frame_generation: Arc<AtomicU64>,
    non_silent_generation: Arc<AtomicU64>,
}

impl WindowsPcmHub {
    pub fn new(audio_format: Ap2AudioFormat) -> Self {
        let byte_rate =
            audio_format.sample_rate as usize * audio_format.input_bytes_per_frame();
        let ring_capacity = (byte_rate.saturating_mul(4)).max(1 << 20);
        Self {
            ring: Arc::new(Mutex::new(Pcm352Chunker::new_with_bytes_per_frame(
                audio_format.input_bytes_per_frame(),
            ))),
            ring_capacity,
            flush_generation: Arc::new(AtomicU64::new(0)),
            flush_ack_generation: Arc::new(AtomicU64::new(0)),
            audio_ready: Arc::new(AtomicBool::new(false)),
            source_present: Arc::new(AtomicBool::new(false)),
            capture_frame_generation: Arc::new(AtomicU64::new(0)),
            non_silent_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn ring(&self) -> Arc<Mutex<Pcm352Chunker>> {
        Arc::clone(&self.ring)
    }

    pub fn ring_capacity(&self) -> usize {
        self.ring_capacity
    }

    pub fn flush_generation(&self) -> u64 {
        self.flush_generation.load(Ordering::SeqCst)
    }

    pub fn request_flush(&self) -> u64 {
        self.flush_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn acknowledge_flush(&self, generation: u64) {
        self.flush_ack_generation.store(generation, Ordering::SeqCst);
    }

    pub fn flush_ack_generation(&self) -> u64 {
        self.flush_ack_generation.load(Ordering::SeqCst)
    }

    pub fn audio_ready(&self) -> bool {
        self.audio_ready.load(Ordering::SeqCst)
    }

    pub fn set_audio_ready(&self, ready: bool) {
        self.audio_ready.store(ready, Ordering::SeqCst);
    }

    pub fn source_present(&self) -> bool {
        self.source_present.load(Ordering::SeqCst)
    }

    pub fn reset_source_present(&self) {
        self.source_present.store(false, Ordering::SeqCst);
    }

    /// Returns true only for the first source-present transition.
    pub fn note_non_silent_packet(&self) -> bool {
        self.non_silent_generation.fetch_add(1, Ordering::SeqCst);
        !self.source_present.swap(true, Ordering::SeqCst)
    }

    pub fn note_capture_frames(&self, frames: usize) {
        if frames != 0 {
            self.capture_frame_generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn capture_frame_generation(&self) -> u64 {
        self.capture_frame_generation.load(Ordering::SeqCst)
    }

    pub fn non_silent_generation(&self) -> u64 {
        self.non_silent_generation.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_frames_keep_capture_alive_without_claiming_source_presence() {
        let hub = WindowsPcmHub::new(Ap2AudioFormat::ALAC_44100_16_STEREO);
        hub.note_capture_frames(480);
        assert_eq!(hub.capture_frame_generation(), 1);
        assert_eq!(hub.non_silent_generation(), 0);
        assert!(!hub.source_present());
    }

    #[test]
    fn non_silent_edge_is_separate_from_capture_generation() {
        let hub = WindowsPcmHub::new(Ap2AudioFormat::ALAC_44100_16_STEREO);
        assert!(hub.note_non_silent_packet());
        assert!(!hub.note_non_silent_packet());
        assert!(hub.source_present());
        assert_eq!(hub.non_silent_generation(), 2);
        assert_eq!(hub.capture_frame_generation(), 0);
    }

    #[test]
    fn flush_request_and_ack_are_distinct() {
        let hub = WindowsPcmHub::new(Ap2AudioFormat::ALAC_44100_16_STEREO);
        let generation = hub.request_flush();
        assert_eq!(generation, 1);
        assert_eq!(hub.flush_ack_generation(), 0);
        hub.acknowledge_flush(generation);
        assert_eq!(hub.flush_ack_generation(), generation);
    }

    #[test]
    fn hub_keeps_msa_bounded_ring_floor() {
        let hub = WindowsPcmHub::new(Ap2AudioFormat::ALAC_44100_16_STEREO);
        assert!(hub.ring_capacity() >= 1 << 20);
        assert_eq!(
            hub.ring().lock().unwrap().packet_bytes(),
            352 * Ap2AudioFormat::ALAC_44100_16_STEREO.input_bytes_per_frame()
        );
    }
}
