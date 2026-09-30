//! Concrete source-faithful ALAC and ChaCha20-Poly1305 adapters for native AP2 SOLO.
//! Pinned to music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128.

use crate::native_media::{AlacEncoder, AudioCipher, CHACHA_TAG_SIZE};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};

pub const ALAC_FRAMES_PER_CHUNK: usize = 352;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    UnsupportedFormat,
    InvalidPcm,
    BackendUnavailable,
    BackendCreateFailed,
    BackendEncodeFailed(i32),
    Encrypt,
}

pub fn input_bytes_per_frame(bit_depth: u16, channels: u16) -> Result<usize, CodecError> {
    if channels == 0 {
        return Err(CodecError::UnsupportedFormat);
    }
    let sample_bytes = if bit_depth <= 16 { 2 } else if bit_depth == 24 { 4 } else {
        return Err(CodecError::UnsupportedFormat);
    };
    Ok(sample_bytes * channels as usize)
}

/// Mirrors MSA pad_final_pcm_chunk: align to a complete PCM frame, discard any
/// incomplete trailing bytes, then silence-pad the transport chunk to 352 frames.
pub fn pad_final_pcm_chunk(
    input: &[u8],
    bit_depth: u16,
    channels: u16,
) -> Result<Vec<u8>, CodecError> {
    let bpf = input_bytes_per_frame(bit_depth, channels)?;
    let chunk_bytes = ALAC_FRAMES_PER_CHUNK * bpf;
    if input.len() > chunk_bytes {
        return Err(CodecError::InvalidPcm);
    }
    let aligned = input.len() - input.len() % bpf;
    let mut out = vec![0u8; chunk_bytes];
    out[..aligned].copy_from_slice(&input[..aligned]);
    Ok(out)
}

fn encode_alac_raw_16_stereo_352(pcm: &[u8]) -> Result<Vec<u8>, CodecError> {
    const BPF: usize = 4;
    const PCM_BYTES: usize = ALAC_FRAMES_PER_CHUNK * BPF;
    if pcm.len() != PCM_BYTES {
        return Err(CodecError::InvalidPcm);
    }

    let bsize = ALAC_FRAMES_PER_CHUNK as u32;
    let mut out = Vec::with_capacity(PCM_BYTES + 16);
    out.push(1 << 5);
    out.push(0);
    out.push((1 << 4) | (1 << 1) | (((bsize & 0x8000_0000) >> 31) as u8));
    out.push((((bsize & 0x7f80_0000) << 1) >> 24) as u8);
    out.push((((bsize & 0x007f_8000) << 1) >> 16) as u8);
    out.push((((bsize & 0x0000_7f80) << 1) >> 8) as u8);

    let first = u32::from_le_bytes(pcm[0..4].try_into().unwrap());
    let mut seventh = ((bsize & 0x0000_007f) << 1) as u8;
    seventh |= ((first & 0x0000_8000) >> 15) as u8;
    out.push(seventh);

    for frame_index in 0..ALAC_FRAMES_PER_CHUNK {
        let off = frame_index * BPF;
        let word = u32::from_le_bytes(pcm[off..off + BPF].try_into().unwrap());

        out.push(((word & 0x0000_7f80) >> 7) as u8);
        out.push((((word & 0x0000_007f) << 1) | ((word & 0x8000_0000) >> 31)) as u8);
        out.push(((word & 0x7f80_0000) >> 23) as u8);

        let next_left_sign = if frame_index + 1 < ALAC_FRAMES_PER_CHUNK {
            let next_off = (frame_index + 1) * BPF;
            let next = u32::from_le_bytes(pcm[next_off..next_off + BPF].try_into().unwrap());
            ((next & 0x0000_8000) >> 15) as u8
        } else {
            0
        };
        out.push((((word & 0x007f_0000) >> 15) as u8) | next_left_sign);
    }

    if let Some(last) = out.last_mut() {
        *last |= 1;
    }
    out.push((7 >> 1) << 6);
    Ok(out)
}

fn truncate_s32le_to_s24le(input: &[u8]) -> Result<Vec<u8>, CodecError> {
    if input.len() % 4 != 0 {
        return Err(CodecError::InvalidPcm);
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for sample in input.chunks_exact(4) {
        out.extend_from_slice(&sample[1..4]);
    }
    Ok(out)
}

#[cfg(windows)]
mod native24 {
    use super::{truncate_s32le_to_s24le, CodecError, ALAC_FRAMES_PER_CHUNK};
    use libloading::Library;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    type CreateFn = unsafe extern "C" fn(i32) -> *mut c_void;
    type EncodeFn = unsafe extern "C" fn(*mut c_void, *const u8, i32, *mut u8, i32) -> i32;
    type DestroyFn = unsafe extern "C" fn(*mut c_void);

    pub struct Alac24 {
        _library: Library,
        encoder: NonNull<c_void>,
        encode: EncodeFn,
        destroy: DestroyFn,
    }

    unsafe impl Send for Alac24 {}

    impl Alac24 {
        pub fn open(sample_rate: u32) -> Result<Self, CodecError> {
            if sample_rate != 44_100 && sample_rate != 48_000 {
                return Err(CodecError::UnsupportedFormat);
            }
            let library = unsafe {
                Library::new("sairplay-alac24.dll").map_err(|_| CodecError::BackendUnavailable)?
            };
            let create: CreateFn = unsafe {
                *library.get::<CreateFn>(b"sairplay_alac24_create\0")
                    .map_err(|_| CodecError::BackendUnavailable)?
            };
            let encode: EncodeFn = unsafe {
                *library.get::<EncodeFn>(b"sairplay_alac24_encode_352\0")
                    .map_err(|_| CodecError::BackendUnavailable)?
            };
            let destroy: DestroyFn = unsafe {
                *library.get::<DestroyFn>(b"sairplay_alac24_destroy\0")
                    .map_err(|_| CodecError::BackendUnavailable)?
            };
            let encoder = NonNull::new(unsafe { create(sample_rate as i32) })
                .ok_or(CodecError::BackendCreateFailed)?;
            Ok(Self { _library: library, encoder, encode, destroy })
        }

        pub fn encode(&mut self, pcm_s32le: &[u8]) -> Result<Vec<u8>, CodecError> {
            const INPUT_BYTES: usize = ALAC_FRAMES_PER_CHUNK * 2 * 4;
            if pcm_s32le.len() != INPUT_BYTES {
                return Err(CodecError::InvalidPcm);
            }
            let packed = truncate_s32le_to_s24le(pcm_s32le)?;
            let mut output = vec![0u8; 8192];
            let encoded = unsafe {
                (self.encode)(
                    self.encoder.as_ptr(),
                    packed.as_ptr(),
                    packed.len() as i32,
                    output.as_mut_ptr(),
                    output.len() as i32,
                )
            };
            if encoded <= 0 {
                return Err(CodecError::BackendEncodeFailed(encoded));
            }
            output.truncate(encoded as usize);
            Ok(output)
        }
    }

    impl Drop for Alac24 {
        fn drop(&mut self) {
            unsafe { (self.destroy)(self.encoder.as_ptr()) };
        }
    }
}

pub enum NativeAlacEncoder {
    Raw16,
    #[cfg(windows)]
    Alac24(native24::Alac24),
}

impl NativeAlacEncoder {
    pub fn open(sample_rate: u32, bit_depth: u16, channels: u16) -> Result<Self, CodecError> {
        if channels != 2 || (sample_rate != 44_100 && sample_rate != 48_000) {
            return Err(CodecError::UnsupportedFormat);
        }
        match bit_depth {
            16 => Ok(Self::Raw16),
            24 => {
                #[cfg(windows)]
                { Ok(Self::Alac24(native24::Alac24::open(sample_rate)?)) }
                #[cfg(not(windows))]
                { let _ = sample_rate; Err(CodecError::BackendUnavailable) }
            }
            _ => Err(CodecError::UnsupportedFormat),
        }
    }
}

impl AlacEncoder for NativeAlacEncoder {
    type Error = CodecError;

    fn encode(&mut self, pcm: &[u8], frames: u32) -> Result<Vec<u8>, Self::Error> {
        if frames as usize != ALAC_FRAMES_PER_CHUNK {
            return Err(CodecError::InvalidPcm);
        }
        match self {
            Self::Raw16 => encode_alac_raw_16_stereo_352(pcm),
            #[cfg(windows)]
            Self::Alac24(enc) => enc.encode(pcm),
        }
    }
}

pub struct ChaChaAudioCipher {
    cipher: ChaCha20Poly1305,
}

impl ChaChaAudioCipher {
    pub fn new(audio_key: [u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&audio_key)) }
    }
}

impl AudioCipher for ChaChaAudioCipher {
    type Error = CodecError;

    fn seal(
        &mut self,
        nonce: &[u8; 12],
        aad: &[u8],
        plain: &[u8],
    ) -> Result<(Vec<u8>, [u8; CHACHA_TAG_SIZE]), Self::Error> {
        if aad.len() != 8 {
            return Err(CodecError::Encrypt);
        }
        let mut sealed = self.cipher
            .encrypt(Nonce::from_slice(nonce), Payload { msg: plain, aad })
            .map_err(|_| CodecError::Encrypt)?;
        if sealed.len() < CHACHA_TAG_SIZE {
            return Err(CodecError::Encrypt);
        }
        let tag_at = sealed.len() - CHACHA_TAG_SIZE;
        let tag_vec = sealed.split_off(tag_at);
        let mut tag = [0u8; CHACHA_TAG_SIZE];
        tag.copy_from_slice(&tag_vec);
        Ok((sealed, tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_chunk_drops_incomplete_pcm_tail_and_pads_to_352_frames() {
        let input = vec![0x55; 41];
        let out = pad_final_pcm_chunk(&input, 16, 2).unwrap();
        assert_eq!(out.len(), 352 * 4);
        assert_eq!(&out[..40], &input[..40]);
        assert_eq!(out[40], 0);
    }

    #[test]
    fn raw_16_alac_matches_msa_packet_shape() {
        let pcm = vec![0u8; 352 * 4];
        let encoded = encode_alac_raw_16_stereo_352(&pcm).unwrap();
        assert_eq!(encoded.len(), 7 + 352 * 4 + 1);
        assert_eq!(&encoded[..3], &[0x20, 0x00, 0x12]);
        assert_eq!(encoded[encoded.len() - 2] & 1, 1);
        assert_eq!(*encoded.last().unwrap(), 0xc0);
    }

    #[test]
    fn s32_carrier_truncates_to_packed_s24le_exactly() {
        let input = [0x11,0x22,0x33,0x44, 0xaa,0xbb,0xcc,0xdd];
        assert_eq!(
            truncate_s32le_to_s24le(&input).unwrap(),
            [0x22,0x33,0x44, 0xbb,0xcc,0xdd]
        );
    }

    #[test]
    fn audio_cipher_returns_ciphertext_and_detached_tag() {
        let mut c = ChaChaAudioCipher::new([0x11; 32]);
        let (ct, tag) = c.seal(&[0; 12], &[1,2,3,4,5,6,7,8], b"hello").unwrap();
        assert_eq!(ct.len(), 5);
        assert_ne!(ct, b"hello");
        assert_ne!(tag, [0u8; 16]);
    }
}
