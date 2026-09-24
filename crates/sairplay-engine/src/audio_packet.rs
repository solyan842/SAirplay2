use crate::RtpState;
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    ChaCha20Poly1305, Key, Nonce, Tag,
};

#[derive(Debug)]
pub enum AudioPacketError {
    Encrypt,
}

pub fn build_audio_nonce(sequence: u16) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    // Source convention: memcpy(nonce + 4, &seqnum, 2).
    // On Windows/x86 this is little-endian host order; make it explicit.
    nonce[4..6].copy_from_slice(&sequence.to_le_bytes());
    nonce
}

pub fn build_encrypted_realtime_packet(
    state: &RtpState,
    alac_payload: &[u8],
    audio_key: &[u8; 32],
) -> Result<Vec<u8>, AudioPacketError> {
    let header = state.header();
    let nonce_bytes = build_audio_nonce(state.sequence);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(audio_key));
    let mut ciphertext = alac_payload.to_vec();

    let tag: Tag = cipher
        .encrypt_in_place_detached(
            Nonce::from_slice(&nonce_bytes),
            &header[4..12],
            &mut ciphertext,
        )
        .map_err(|_| AudioPacketError::Encrypt)?;

    let mut out = Vec::with_capacity(12 + ciphertext.len() + 16 + 8);
    out.extend_from_slice(&header);
    out.extend_from_slice(&ciphertext);
    out.extend_from_slice(tag.as_slice());
    out.extend_from_slice(&nonce_bytes[4..12]);
    Ok(out)
}

pub const RTP_PAYLOAD_TYPE_BUFFERED: u8 = 103;

pub fn build_buffered_audio_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..12].copy_from_slice(&counter.to_le_bytes());
    nonce
}

/// Build one pinned-MSA buffered type-103 TCP frame:
/// [2B BE total length][12B RTP PT103][ciphertext][16B tag][8B nonce suffix].
/// The two-byte length counts itself.
pub fn build_encrypted_buffered_frame(
    state: &RtpState,
    alac_payload: &[u8],
    audio_key: &[u8; 32],
    nonce_counter: u64,
) -> Result<Vec<u8>, AudioPacketError> {
    let mut header = state.header();
    header[1] = RTP_PAYLOAD_TYPE_BUFFERED
        | if state.first_packet { crate::RTP_MARKER_BIT } else { 0 };

    let nonce_bytes = build_buffered_audio_nonce(nonce_counter);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(audio_key));
    let mut ciphertext = alac_payload.to_vec();
    let tag: Tag = cipher
        .encrypt_in_place_detached(
            Nonce::from_slice(&nonce_bytes),
            &header[4..12],
            &mut ciphertext,
        )
        .map_err(|_| AudioPacketError::Encrypt)?;

    let payload_len = 12 + ciphertext.len() + 16 + 8;
    let total_len = payload_len + 2;
    if total_len > u16::MAX as usize {
        return Err(AudioPacketError::Encrypt);
    }

    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&(total_len as u16).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&ciphertext);
    out.extend_from_slice(tag.as_slice());
    out.extend_from_slice(&nonce_bytes[4..12]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::AeadInPlace;

    #[test]
    fn buffered_nonce_uses_full_64bit_little_endian_counter() {
        let nonce = build_buffered_audio_nonce(0x1122334455667788);
        assert_eq!(&nonce[..4], &[0, 0, 0, 0]);
        assert_eq!(&nonce[4..12], &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn buffered_wire_shape_is_be_length_pt103_ciphertext_tag_nonce() {
        let state = RtpState::new(0x1234, 0x11223344, 0);
        let key = [0x55u8; 32];
        let payload = b"buffered-alac";
        let frame = build_encrypted_buffered_frame(&state, payload, &key, 9).unwrap();

        assert_eq!(u16::from_be_bytes([frame[0], frame[1]]) as usize, frame.len());
        assert_eq!(frame[2], 0x80);
        assert_eq!(frame[3], 0xE7);
        assert_eq!(&frame[4..6], &0x1234u16.to_be_bytes());
        assert_eq!(&frame[frame.len() - 8..], &9u64.to_le_bytes());
        assert_eq!(frame.len(), 2 + 12 + payload.len() + 16 + 8);
    }

    #[test]
    fn nonce_places_sequence_at_offset_four_little_endian() {
        let nonce = build_audio_nonce(0x1234);
        assert_eq!(&nonce[..4], &[0, 0, 0, 0]);
        assert_eq!(&nonce[4..6], &[0x34, 0x12]);
        assert_eq!(&nonce[6..], &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn wire_shape_is_header_ciphertext_tag_and_eight_byte_nonce_suffix() {
        let state = RtpState::new(0x1234, 0x11223344, 0x55667788);
        let key = [0x42u8; 32];
        let payload = b"encoded-alac-frame";

        let packet = build_encrypted_realtime_packet(&state, payload, &key).unwrap();

        assert_eq!(&packet[..12], &state.header());
        assert_eq!(packet.len(), 12 + payload.len() + 16 + 8);

        let nonce = build_audio_nonce(state.sequence);
        assert_eq!(&packet[packet.len() - 8..], &nonce[4..12]);
    }

    #[test]
    fn receiver_can_decrypt_using_timestamp_ssrc_as_aad() {
        let state = RtpState::new(7, 352, 0xAABBCCDD);
        let key = [0x24u8; 32];
        let payload = b"alac-payload-bytes";

        let packet = build_encrypted_realtime_packet(&state, payload, &key).unwrap();

        let header = &packet[..12];
        let tag_start = packet.len() - 8 - 16;
        let mut ciphertext = packet[12..tag_start].to_vec();
        let tag = Tag::from_slice(&packet[tag_start..tag_start + 16]);

        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&packet[packet.len() - 8..]);

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                &header[4..12],
                &mut ciphertext,
                tag,
            )
            .unwrap();

        assert_eq!(ciphertext, payload);
    }

    #[test]
    fn changing_rtp_timestamp_breaks_authentication() {
        let state = RtpState::new(8, 704, 0x01020304);
        let key = [0x33u8; 32];
        let payload = b"frame";

        let packet = build_encrypted_realtime_packet(&state, payload, &key).unwrap();
        let mut bad_header = packet[..12].to_vec();
        bad_header[7] ^= 0x01;

        let tag_start = packet.len() - 8 - 16;
        let mut ciphertext = packet[12..tag_start].to_vec();
        let tag = Tag::from_slice(&packet[tag_start..tag_start + 16]);

        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&packet[packet.len() - 8..]);

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        assert!(cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                &bad_header[4..12],
                &mut ciphertext,
                tag,
            )
            .is_err());
    }
}
