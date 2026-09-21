use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use hkdf::Hkdf;
use sha2::Sha512;

pub const HAP_MAX_FRAME_SIZE: usize = 1024;
pub const HAP_TAG_SIZE: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HapCryptoError {
    Hkdf,
    InvalidFrame,
    Encrypt,
    Decrypt,
}

pub fn derive_control_keys(shared_secret: &[u8]) -> Result<([u8; 32], [u8; 32]), HapCryptoError> {
    let hk = Hkdf::<Sha512>::new(Some(b"Control-Salt"), shared_secret);

    let mut write_key = [0u8; 32];
    hk.expand(b"Control-Write-Encryption-Key", &mut write_key)
        .map_err(|_| HapCryptoError::Hkdf)?;

    let mut read_key = [0u8; 32];
    hk.expand(b"Control-Read-Encryption-Key", &mut read_key)
        .map_err(|_| HapCryptoError::Hkdf)?;

    Ok((write_key, read_key))
}

pub fn hap_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

pub struct HapControlCipher {
    write: ChaCha20Poly1305,
    read: ChaCha20Poly1305,
    write_counter: u64,
    read_counter: u64,
}

impl HapControlCipher {
    pub fn new(write_key: [u8; 32], read_key: [u8; 32]) -> Self {
        Self {
            write: ChaCha20Poly1305::new(Key::from_slice(&write_key)),
            read: ChaCha20Poly1305::new(Key::from_slice(&read_key)),
            write_counter: 0,
            read_counter: 0,
        }
    }

    pub fn write_counter(&self) -> u64 {
        self.write_counter
    }

    pub fn read_counter(&self) -> u64 {
        self.read_counter
    }

    pub fn set_read_counter(&mut self, counter: u64) {
        self.read_counter = counter;
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, HapCryptoError> {
        if plaintext.is_empty() {
            return Ok(Vec::new());
        }

        let frames = plaintext.len().div_ceil(HAP_MAX_FRAME_SIZE);
        let mut out = Vec::with_capacity(plaintext.len() + frames * (2 + HAP_TAG_SIZE));

        for chunk in plaintext.chunks(HAP_MAX_FRAME_SIZE) {
            let len_bytes = (chunk.len() as u16).to_le_bytes();
            let nonce_bytes = hap_nonce(self.write_counter);
            let encrypted = self
                .write
                .encrypt(
                    Nonce::from_slice(&nonce_bytes),
                    Payload {
                        msg: chunk,
                        aad: &len_bytes,
                    },
                )
                .map_err(|_| HapCryptoError::Encrypt)?;

            out.extend_from_slice(&len_bytes);
            out.extend_from_slice(&encrypted);
            self.write_counter = self.write_counter.wrapping_add(1);
        }

        Ok(out)
    }

    pub fn decrypt(&mut self, wire: &[u8]) -> Result<Vec<u8>, HapCryptoError> {
        let mut pos = 0usize;
        let mut out = Vec::new();

        while pos < wire.len() {
            if wire.len() - pos < 2 {
                return Err(HapCryptoError::InvalidFrame);
            }

            let len_bytes = [wire[pos], wire[pos + 1]];
            let plain_len = u16::from_le_bytes(len_bytes) as usize;
            pos += 2;

            if plain_len > HAP_MAX_FRAME_SIZE {
                return Err(HapCryptoError::InvalidFrame);
            }

            let cipher_len = plain_len + HAP_TAG_SIZE;
            if wire.len() - pos < cipher_len {
                return Err(HapCryptoError::InvalidFrame);
            }

            let nonce_bytes = hap_nonce(self.read_counter);
            let plaintext = self
                .read
                .decrypt(
                    Nonce::from_slice(&nonce_bytes),
                    Payload {
                        msg: &wire[pos..pos + cipher_len],
                        aad: &len_bytes,
                    },
                )
                .map_err(|_| HapCryptoError::Decrypt)?;

            if plaintext.len() != plain_len {
                return Err(HapCryptoError::InvalidFrame);
            }

            out.extend_from_slice(&plaintext);
            pos += cipher_len;
            self.read_counter = self.read_counter.wrapping_add(1);
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_matches_cross_checked_control_key_vector() {
        let secret: Vec<u8> = (0u8..32).collect();
        let (write, read) = derive_control_keys(&secret).unwrap();

        assert_eq!(
            hex(&write),
            "c3ca130c7033dbe5e7ff7f91d117ead869bac476994c7a48ca170c111136ed96"
        );
        assert_eq!(
            hex(&read),
            "c09403ef8aa6c5045cbd8cf9bf3e665b2caed623af2be0e87c8f80f519914d3d"
        );
    }

    #[test]
    fn nonce_is_four_zero_bytes_plus_little_endian_counter() {
        assert_eq!(
            hap_nonce(0x0102030405060708),
            [0,0,0,0,0x08,0x07,0x06,0x05,0x04,0x03,0x02,0x01]
        );
    }

    #[test]
    fn roundtrip_single_frame() {
        let key = [0x11u8; 32];
        let mut sender = HapControlCipher::new(key, key);
        let mut receiver = HapControlCipher::new(key, key);

        let plaintext = b"GET /info RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let wire = sender.encrypt(plaintext).unwrap();
        let decoded = receiver.decrypt(&wire).unwrap();

        assert_eq!(decoded, plaintext);
        assert_eq!(sender.write_counter(), 1);
        assert_eq!(receiver.read_counter(), 1);
    }

    #[test]
    fn roundtrip_splits_more_than_1024_bytes_into_multiple_frames() {
        let key = [0x22u8; 32];
        let mut sender = HapControlCipher::new(key, key);
        let mut receiver = HapControlCipher::new(key, key);
        let plaintext = vec![0x5a; 2500];

        let wire = sender.encrypt(&plaintext).unwrap();
        let decoded = receiver.decrypt(&wire).unwrap();

        assert_eq!(decoded, plaintext);
        assert_eq!(sender.write_counter(), 3);
        assert_eq!(receiver.read_counter(), 3);
    }

    #[test]
    fn tampered_length_aad_fails_authentication() {
        let key = [0x33u8; 32];
        let mut sender = HapControlCipher::new(key, key);
        let mut receiver = HapControlCipher::new(key, key);
        let mut wire = sender.encrypt(b"hello").unwrap();

        wire[0] = 4;
        assert!(matches!(receiver.decrypt(&wire), Err(HapCryptoError::Decrypt)));
    }

    #[test]
    fn read_counter_can_be_restored_for_retry_safe_decryption() {
        let key = [0x44u8; 32];
        let mut sender = HapControlCipher::new(key, key);
        let mut receiver = HapControlCipher::new(key, key);

        let first = sender.encrypt(b"one").unwrap();
        assert_eq!(receiver.decrypt(&first).unwrap(), b"one");
        let saved = receiver.read_counter();

        let second = sender.encrypt(b"two").unwrap();
        let corrupted = &second[..second.len() - 1];
        assert!(receiver.decrypt(corrupted).is_err());

        receiver.set_read_counter(saved);
        assert_eq!(receiver.decrypt(&second).unwrap(), b"two");
    }

    fn hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }
}
