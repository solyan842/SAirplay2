//! Windows loopback PCM bridge.
//!
//! Music Assistant never asks cliairplay to reconfigure an OS audio endpoint.
//! MA converts every source to the exact raw PCM format required by cliairplay
//! before writing it to stdin. On Windows, WASAPI loopback therefore captures
//! the endpoint's real shared-mode mix format and this bridge performs the
//! equivalent sample-format/rate conversion in-process.

use crate::{Ap2AudioFormat, Pcm352Chunker};
use rubato::{FftFixedInOut, Resampler};
use std::collections::VecDeque;
use std::fmt;

const RESAMPLE_CHUNK_FRAMES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSampleKind {
    I16,
    I24,
    I32,
    F32,
    F64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputPcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bytes_per_frame: usize,
    pub bits_per_sample: u16,
    pub valid_bits_per_sample: u16,
    pub kind: InputSampleKind,
}

impl InputPcmFormat {
    pub fn bytes_per_sample(self) -> usize {
        self.bytes_per_frame / self.channels.max(1) as usize
    }

    pub fn describe(self) -> String {
        let sample = match self.kind {
            InputSampleKind::I16 => "s16le",
            InputSampleKind::I24 => "s24le",
            InputSampleKind::I32 => {
                if self.valid_bits_per_sample < 32 {
                    "s32le-carrier"
                } else {
                    "s32le"
                }
            }
            InputSampleKind::F32 => "f32le",
            InputSampleKind::F64 => "f64le",
        };
        format!(
            "{} Hz / {} / {}ch ({} valid bits)",
            self.sample_rate, sample, self.channels, self.valid_bits_per_sample
        )
    }
}

#[derive(Debug)]
pub(crate) enum PcmBridgeError {
    Unsupported(String),
    InvalidInput(String),
    Resampler(String),
}

impl fmt::Display for PcmBridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(v) => write!(f, "{v}"),
            Self::InvalidInput(v) => write!(f, "{v}"),
            Self::Resampler(v) => write!(f, "{v}"),
        }
    }
}

impl std::error::Error for PcmBridgeError {}

pub(crate) struct PcmBridge {
    input: InputPcmFormat,
    target: Ap2AudioFormat,
    resampler: Option<FftFixedInOut<f32>>,
    pending: [VecDeque<f32>; 2],
    resample_in: Vec<Vec<f32>>,
    resample_out: Vec<Vec<f32>>,
}

impl PcmBridge {
    pub fn new(input: InputPcmFormat, target: Ap2AudioFormat) -> Result<Self, PcmBridgeError> {
        if target.channels != 2 {
            return Err(PcmBridgeError::Unsupported(format!(
                "MSA Windows PCM bridge requires stereo target, got {} channels",
                target.channels
            )));
        }
        if target.bit_depth != 16 && target.bit_depth != 24 {
            return Err(PcmBridgeError::Unsupported(format!(
                "MSA Windows PCM bridge supports 16/24-bit target, got {}",
                target.bit_depth
            )));
        }
        if input.sample_rate == 0 || input.channels == 0 {
            return Err(PcmBridgeError::InvalidInput(
                "Windows mix format has zero sample-rate/channels".into(),
            ));
        }
        // Do not silently invent a surround downmix. MSA's FFmpeg path has an
        // explicit channel-conversion stage; this adapter currently guarantees
        // source-faithful mono/stereo only.
        if input.channels > 2 {
            return Err(PcmBridgeError::Unsupported(format!(
                "Windows mix has {} channels; refusing an implicit surround downmix",
                input.channels
            )));
        }
        let sample_bytes = input.bytes_per_sample();
        if sample_bytes == 0
            || sample_bytes * input.channels as usize != input.bytes_per_frame
        {
            return Err(PcmBridgeError::InvalidInput(format!(
                "invalid Windows mix block alignment: {} bytes/frame for {} channels",
                input.bytes_per_frame, input.channels
            )));
        }

        let mut resampler = None;
        let mut resample_in = vec![Vec::<f32>::new(), Vec::<f32>::new()];
        let mut resample_out = vec![Vec::<f32>::new(), Vec::<f32>::new()];
        if input.sample_rate != target.sample_rate {
            let r = FftFixedInOut::<f32>::new(
                input.sample_rate as usize,
                target.sample_rate as usize,
                RESAMPLE_CHUNK_FRAMES,
                2,
            )
            .map_err(|e| PcmBridgeError::Resampler(format!(
                "cannot create {} -> {} Hz resampler: {e}",
                input.sample_rate, target.sample_rate
            )))?;
            resample_in = r.input_buffer_allocate(true);
            resample_out = r.output_buffer_allocate(true);
            resampler = Some(r);
        }

        Ok(Self {
            input,
            target,
            resampler,
            pending: [VecDeque::new(), VecDeque::new()],
            resample_in,
            resample_out,
        })
    }

    pub fn input_format(&self) -> InputPcmFormat {
        self.input
    }

    pub fn target_format(&self) -> Ap2AudioFormat {
        self.target
    }

    pub fn reset(&mut self) {
        self.pending[0].clear();
        self.pending[1].clear();
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }
    }

    pub fn push_packet(
        &mut self,
        bytes: &[u8],
        frames: usize,
        silent: bool,
        chunker: &mut Pcm352Chunker,
    ) -> Result<usize, PcmBridgeError> {
        if frames == 0 {
            return Ok(0);
        }
        let expected = frames
            .checked_mul(self.input.bytes_per_frame)
            .ok_or_else(|| PcmBridgeError::InvalidInput("Windows mix packet size overflow".into()))?;
        if !silent && bytes.len() != expected {
            return Err(PcmBridgeError::InvalidInput(format!(
                "Windows mix packet length {} does not match {} frames x {} bytes/frame",
                bytes.len(), frames, self.input.bytes_per_frame
            )));
        }

        if silent {
            self.pending[0].extend(std::iter::repeat(0.0).take(frames));
            self.pending[1].extend(std::iter::repeat(0.0).take(frames));
        } else {
            self.decode_to_stereo(bytes, frames)?;
        }

        self.flush_available(chunker)
    }

    fn decode_to_stereo(&mut self, bytes: &[u8], frames: usize) -> Result<(), PcmBridgeError> {
        let channels = self.input.channels as usize;
        let sample_bytes = self.input.bytes_per_sample();
        for frame in 0..frames {
            let frame_base = frame * self.input.bytes_per_frame;
            let left = decode_sample(
                self.input.kind,
                &bytes[frame_base..frame_base + sample_bytes],
            )?;
            let right = if channels == 1 {
                left
            } else {
                let off = frame_base + sample_bytes;
                decode_sample(
                    self.input.kind,
                    &bytes[off..off + sample_bytes],
                )?
            };
            self.pending[0].push_back(left);
            self.pending[1].push_back(right);
        }
        Ok(())
    }

    fn flush_available(&mut self, chunker: &mut Pcm352Chunker) -> Result<usize, PcmBridgeError> {
        let mut produced_total = 0usize;

        if self.resampler.is_none() {
            let frames = self.pending[0].len().min(self.pending[1].len());
            if frames == 0 {
                return Ok(0);
            }
            let mut out = Vec::with_capacity(frames * self.target.input_bytes_per_frame());
            for _ in 0..frames {
                let l = self.pending[0].pop_front().expect("length checked");
                let r = self.pending[1].pop_front().expect("length checked");
                encode_target_frame(self.target.bit_depth, l, r, &mut out);
            }
            chunker.push(&out);
            return Ok(frames);
        }

        loop {
            let need = self
                .resampler
                .as_ref()
                .expect("checked")
                .input_frames_next();
            if self.pending[0].len() < need || self.pending[1].len() < need {
                break;
            }
            for ch in 0..2 {
                for i in 0..need {
                    self.resample_in[ch][i] =
                        self.pending[ch].pop_front().expect("length checked");
                }
            }
            let (_, produced) = self
                .resampler
                .as_mut()
                .expect("checked")
                .process_into_buffer(&self.resample_in, &mut self.resample_out, None)
                .map_err(|e| PcmBridgeError::Resampler(format!("realtime resample failed: {e}")))?;

            let mut out = Vec::with_capacity(produced * self.target.input_bytes_per_frame());
            for i in 0..produced {
                encode_target_frame(
                    self.target.bit_depth,
                    self.resample_out[0][i],
                    self.resample_out[1][i],
                    &mut out,
                );
            }
            chunker.push(&out);
            produced_total = produced_total.saturating_add(produced);
        }

        Ok(produced_total)
    }
}

fn decode_sample(kind: InputSampleKind, bytes: &[u8]) -> Result<f32, PcmBridgeError> {
    let sample = match kind {
        InputSampleKind::I16 => {
            if bytes.len() != 2 {
                return Err(PcmBridgeError::InvalidInput("invalid s16 sample width".into()));
            }
            i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0
        }
        InputSampleKind::I24 => {
            if bytes.len() != 3 {
                return Err(PcmBridgeError::InvalidInput("invalid s24 sample width".into()));
            }
            let raw = i32::from_le_bytes([
                bytes[0],
                bytes[1],
                bytes[2],
                if bytes[2] & 0x80 != 0 { 0xff } else { 0x00 },
            ]);
            raw as f32 / 8_388_608.0
        }
        InputSampleKind::I32 => {
            if bytes.len() != 4 {
                return Err(PcmBridgeError::InvalidInput("invalid s32 sample width".into()));
            }
            i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0
        }
        InputSampleKind::F32 => {
            if bytes.len() != 4 {
                return Err(PcmBridgeError::InvalidInput("invalid f32 sample width".into()));
            }
            f32::from_le_bytes(bytes.try_into().unwrap())
        }
        InputSampleKind::F64 => {
            if bytes.len() != 8 {
                return Err(PcmBridgeError::InvalidInput("invalid f64 sample width".into()));
            }
            f64::from_le_bytes(bytes.try_into().unwrap()) as f32
        }
    };
    Ok(if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 })
}

fn encode_target_frame(bit_depth: u16, left: f32, right: f32, out: &mut Vec<u8>) {
    match bit_depth {
        16 => {
            for sample in [left, right] {
                let value = f32_to_i16(sample);
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        24 => {
            // Exact MSA handoff contract: 24-bit cliairplay input is s32le.
            // The ALAC stage then drops the low byte to packed s24le.
            for sample in [left, right] {
                let value = f32_to_i32(sample);
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        _ => unreachable!("validated target bit depth"),
    }
}

fn f32_to_i16(sample: f32) -> i16 {
    let sample = sample.clamp(-1.0, 1.0);
    if sample <= -1.0 {
        i16::MIN
    } else if sample >= 1.0 {
        i16::MAX
    } else {
        (sample * 32768.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }
}

fn f32_to_i32(sample: f32) -> i32 {
    let sample = sample.clamp(-1.0, 1.0);
    if sample <= -1.0 {
        i32::MIN
    } else if sample >= 1.0 {
        i32::MAX
    } else {
        (sample as f64 * 2_147_483_648.0)
            .round()
            .clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_16(rate: u32) -> Ap2AudioFormat {
        Ap2AudioFormat { sample_rate: rate, bit_depth: 16, channels: 2 }
    }

    #[test]
    fn f32_stereo_converts_to_s16_without_reinterpreting_bytes() {
        let input = InputPcmFormat {
            sample_rate: 44_100,
            channels: 2,
            bytes_per_frame: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            kind: InputSampleKind::F32,
        };
        let mut bridge = PcmBridge::new(input, target_16(44_100)).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.5f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5f32).to_le_bytes());
        let mut chunker = Pcm352Chunker::new();
        assert_eq!(bridge.push_packet(&bytes, 1, false, &mut chunker).unwrap(), 1);
        let out = chunker.pop_packet_padded_silence();
        assert_eq!(i16::from_le_bytes(out[0..2].try_into().unwrap()), 16384);
        assert_eq!(i16::from_le_bytes(out[2..4].try_into().unwrap()), -16384);
    }

    #[test]
    fn mono_is_duplicated_but_surround_is_rejected() {
        let mono = InputPcmFormat {
            sample_rate: 44_100,
            channels: 1,
            bytes_per_frame: 2,
            bits_per_sample: 16,
            valid_bits_per_sample: 16,
            kind: InputSampleKind::I16,
        };
        assert!(PcmBridge::new(mono, target_16(44_100)).is_ok());

        let surround = InputPcmFormat { channels: 6, bytes_per_frame: 24, ..mono };
        assert!(PcmBridge::new(surround, target_16(44_100)).is_err());
    }

    #[test]
    fn rate_48k_to_44k1_resampling_changes_frame_count_not_playback_pitch() {
        let input = InputPcmFormat {
            sample_rate: 48_000,
            channels: 2,
            bytes_per_frame: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            kind: InputSampleKind::F32,
        };
        let mut bridge = PcmBridge::new(input, target_16(44_100)).unwrap();
        let mut chunker = Pcm352Chunker::new();
        let frames = 48_000usize;
        let mut bytes = Vec::with_capacity(frames * 8);
        for i in 0..frames {
            let sample = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin() * 0.5;
            bytes.extend_from_slice(&sample.to_le_bytes());
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let produced = bridge.push_packet(&bytes, frames, false, &mut chunker).unwrap();
        // Fixed FFT chunks leave at most one not-yet-complete input block queued.
        assert!(produced > 43_000 && produced <= 44_100);
        assert_eq!(chunker.pending_bytes() % 4, 0);
    }

    #[test]
    fn reset_discards_only_converter_history() {
        let input = InputPcmFormat {
            sample_rate: 48_000,
            channels: 2,
            bytes_per_frame: 8,
            bits_per_sample: 32,
            valid_bits_per_sample: 32,
            kind: InputSampleKind::F32,
        };
        let mut bridge = PcmBridge::new(input, target_16(44_100)).unwrap();
        let mut chunker = Pcm352Chunker::new();
        let bytes = vec![0u8; 100 * 8];
        assert_eq!(bridge.push_packet(&bytes, 100, false, &mut chunker).unwrap(), 0);
        bridge.reset();
        assert_eq!(bridge.push_packet(&bytes, 100, false, &mut chunker).unwrap(), 0);
    }
}
