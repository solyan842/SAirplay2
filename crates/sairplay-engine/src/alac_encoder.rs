pub const ALAC_FRAMES_PER_PACKET: usize = 352;
pub const ALAC_PCM_BYTES_PER_FRAME: usize = 4;
pub const ALAC_PCM_PACKET_BYTES: usize = ALAC_FRAMES_PER_PACKET * ALAC_PCM_BYTES_PER_FRAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlacEncodeError {
    Empty,
    TooManyFrames,
    MisalignedPcm,
}

/// Source-aligned raw ALAC framing for the locked SAirplay2 baseline:
/// signed 16-bit native/little-endian stereo, 44.1 kHz, max 352 frames.
///
/// This mirrors the 16-bit-stereo pcm_to_alac_raw path used by the current
/// airplay-cli implementation. Short input is zero padded to one 352-frame
/// packet; full baseline input is exactly 1408 PCM bytes.
pub fn encode_alac_16_stereo_352(pcm_le: &[u8]) -> Result<Vec<u8>, AlacEncodeError> {
    if pcm_le.is_empty() {
        return Err(AlacEncodeError::Empty);
    }
    if pcm_le.len() % ALAC_PCM_BYTES_PER_FRAME != 0 {
        return Err(AlacEncodeError::MisalignedPcm);
    }

    let frames = pcm_le.len() / ALAC_PCM_BYTES_PER_FRAME;
    if frames > ALAC_FRAMES_PER_PACKET {
        return Err(AlacEncodeError::TooManyFrames);
    }

    let bsize = ALAC_FRAMES_PER_PACKET as u32;
    let mut out = Vec::with_capacity(ALAC_PCM_PACKET_BYTES + 16);

    out.push(1 << 5);
    out.push(0);
    out.push((1 << 4) | (1 << 1) | (((bsize & 0x8000_0000) >> 31) as u8));
    out.push((((bsize & 0x7f80_0000) << 1) >> 24) as u8);
    out.push((((bsize & 0x007f_8000) << 1) >> 16) as u8);
    out.push((((bsize & 0x0000_7f80) << 1) >> 8) as u8);

    let first = u32::from_le_bytes(pcm_le[0..4].try_into().unwrap());
    let mut seventh = ((bsize & 0x0000_007f) << 1) as u8;
    seventh |= ((first & 0x0000_8000) >> 15) as u8;
    out.push(seventh);

    for frame_index in 0..frames {
        let off = frame_index * 4;
        let word = u32::from_le_bytes(pcm_le[off..off + 4].try_into().unwrap());

        out.push(((word & 0x0000_7f80) >> 7) as u8);
        out.push((((word & 0x0000_007f) << 1) | ((word & 0x8000_0000) >> 31)) as u8);
        out.push(((word & 0x7f80_0000) >> 23) as u8);

        let next_left_sign = if frame_index + 1 < frames {
            let next_off = (frame_index + 1) * 4;
            let next = u32::from_le_bytes(pcm_le[next_off..next_off + 4].try_into().unwrap());
            ((next & 0x0000_8000) >> 15) as u8
        } else {
            0
        };

        out.push((((word & 0x007f_0000) >> 15) as u8) | next_left_sign);
    }

    for _ in frames..ALAC_FRAMES_PER_PACKET {
        out.extend_from_slice(&[0, 0, 0, 0]);
    }

    if let Some(last) = out.last_mut() {
        *last |= 1;
    }
    out.push((7 >> 1) << 6);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_352_frame_packet_has_source_expected_size() {
        let pcm = vec![0u8; ALAC_PCM_PACKET_BYTES];
        let encoded = encode_alac_16_stereo_352(&pcm).unwrap();
        assert_eq!(encoded.len(), 7 + ALAC_PCM_PACKET_BYTES + 1);
        assert_eq!(&encoded[..3], &[0x20, 0x00, 0x12]);
        assert_eq!(*encoded.last().unwrap(), 0xC0);
    }

    #[test]
    fn silence_packet_is_deterministic_and_sets_end_bit() {
        let pcm = vec![0u8; ALAC_PCM_PACKET_BYTES];
        let encoded = encode_alac_16_stereo_352(&pcm).unwrap();
        assert_eq!(encoded[6], 0xC0);
        assert_eq!(encoded[encoded.len() - 2] & 1, 1);
        assert_eq!(encoded[encoded.len() - 1], 0xC0);
    }

    #[test]
    fn short_packet_is_zero_padded_to_352_frames() {
        let pcm = vec![0u8; 10 * ALAC_PCM_BYTES_PER_FRAME];
        let encoded = encode_alac_16_stereo_352(&pcm).unwrap();
        assert_eq!(encoded.len(), 7 + ALAC_PCM_PACKET_BYTES + 1);
    }

    #[test]
    fn rejects_non_stereo_frame_alignment_and_oversize() {
        assert_eq!(
            encode_alac_16_stereo_352(&[0, 1, 2]),
            Err(AlacEncodeError::MisalignedPcm)
        );
        assert_eq!(
            encode_alac_16_stereo_352(&vec![0u8; (ALAC_FRAMES_PER_PACKET + 1) * 4]),
            Err(AlacEncodeError::TooManyFrames)
        );
    }

    #[test]
    fn signed_sample_bits_follow_source_packing() {
        let pcm = [0x00, 0x80, 0xff, 0x7f];
        let encoded = encode_alac_16_stereo_352(&pcm).unwrap();
        assert_eq!(encoded[6] & 1, 1);
        assert_eq!(&encoded[7..11], &[0x00, 0x00, 0xff, 0xfe]);
    }
}
