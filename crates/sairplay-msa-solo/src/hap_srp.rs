use num_bigint::BigUint;
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha512};

pub const SRP_TRANSIENT_PIN: &str = "3939";
const SRP_USERNAME: &str = "Pair-Setup";
const SRP_N_BYTES: usize = 384;

const SRP_N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrpError {
    InvalidServerPublicKey,
    ZeroScramblingParameter,
    InvalidPrivateValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrpClientResult {
    pub public_key_a: Vec<u8>,
    pub proof_m1: [u8; 64],
    pub expected_hamk: [u8; 64],
    pub session_key: [u8; 64],
}

pub fn srp_client_compute(
    salt: &[u8],
    server_public_b: &[u8],
    pin: &str,
) -> Result<SrpClientResult, SrpError> {
    let mut private = [0u8; 32];
    OsRng.fill_bytes(&mut private);
    private[0] |= 0x80;
    srp_client_compute_with_private(salt, server_public_b, pin, &private)
}

pub fn srp_client_compute_with_private(
    salt: &[u8],
    server_public_b: &[u8],
    pin: &str,
    private_a: &[u8],
) -> Result<SrpClientResult, SrpError> {
    let n = modulus();
    let g = BigUint::from(5u8);
    let b_pub = BigUint::from_bytes_be(server_public_b);
    if (&b_pub % &n) == BigUint::from(0u8) {
        return Err(SrpError::InvalidServerPublicKey);
    }

    let a = BigUint::from_bytes_be(private_a);
    if a == BigUint::from(0u8) {
        return Err(SrpError::InvalidPrivateValue);
    }

    let a_pub = g.modpow(&a, &n);
    let a_bytes = minimal_bytes(&a_pub);

    let k = BigUint::from_bytes_be(&sha512(&[pad(&n), pad(&g)].concat()));
    let u = BigUint::from_bytes_be(&sha512(&[pad(&a_pub), pad(&b_pub)].concat()));
    if u == BigUint::from(0u8) {
        return Err(SrpError::ZeroScramblingParameter);
    }

    let inner = sha512(format!("{SRP_USERNAME}:{pin}").as_bytes());
    let mut x_input = Vec::with_capacity(salt.len() + inner.len());
    x_input.extend_from_slice(salt);
    x_input.extend_from_slice(&inner);
    let x = BigUint::from_bytes_be(&sha512(&x_input));

    let gx = g.modpow(&x, &n);
    let kgx = (&k * gx) % &n;
    let base = if b_pub >= kgx {
        (&b_pub - &kgx) % &n
    } else {
        (&b_pub + &n - &kgx) % &n
    };
    let exponent = &a + (&u * &x);
    let s = base.modpow(&exponent, &n);
    let session_key = sha512(&minimal_bytes(&s));

    let hash_n = sha512(&minimal_bytes(&n));
    let hash_g = sha512(&minimal_bytes(&g));
    let mut xor = [0u8; 64];
    for i in 0..64 {
        xor[i] = hash_n[i] ^ hash_g[i];
    }
    let hash_user = sha512(SRP_USERNAME.as_bytes());
    let b_bytes = minimal_bytes(&b_pub);

    let mut m1_input = Vec::new();
    m1_input.extend_from_slice(&xor);
    m1_input.extend_from_slice(&hash_user);
    m1_input.extend_from_slice(salt);
    m1_input.extend_from_slice(&a_bytes);
    m1_input.extend_from_slice(&b_bytes);
    m1_input.extend_from_slice(&session_key);
    let proof_m1 = sha512(&m1_input);

    let mut hamk_input = Vec::new();
    hamk_input.extend_from_slice(&a_bytes);
    hamk_input.extend_from_slice(&proof_m1);
    hamk_input.extend_from_slice(&session_key);
    let expected_hamk = sha512(&hamk_input);

    Ok(SrpClientResult {
        public_key_a: a_bytes,
        proof_m1,
        expected_hamk,
        session_key,
    })
}

fn modulus() -> BigUint {
    BigUint::parse_bytes(SRP_N_HEX.as_bytes(), 16).expect("valid RFC 5054 3072-bit modulus")
}

fn pad(value: &BigUint) -> Vec<u8> {
    let raw = value.to_bytes_be();
    let mut out = vec![0u8; SRP_N_BYTES];
    let start = SRP_N_BYTES.saturating_sub(raw.len());
    out[start..].copy_from_slice(&raw);
    out
}

fn minimal_bytes(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    if bytes.is_empty() { vec![0] } else { bytes }
}

fn sha512(data: &[u8]) -> [u8; 64] {
    let digest = Sha512::digest(data);
    let mut out = [0u8; 64];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
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

    #[test]
    fn deterministic_vector_matches_independent_reference_calculation() {
        let salt = hex_decode("00112233445566778899aabbccddeeff");
        let private_a = hex_decode(
            "8011111111111111111111111111111111111111111111111111111111111111"
        );
        let server_b = hex_decode(concat!(
            "7e0ef28c1b8b6352ac0c508d3ec9994176a6065f0cf23895aa73d51e9c17b27f",
            "06280a599c31c87cf59227861396a013d1546e3b4890c3d164a965dde273d26f6",
            "ebade7a6a2b6b59c228ac78101a7a7e3adbbdf97000d927f06053a9d7915ea90",
            "c87a7ca1e487bc7c1d5cc7d2ded8cec628b8da7acdef8c3541473766ce07eb849",
            "c5516a5bb1b9342e804d3ad83f6268cf7e063ad9daebc03d48d3e6ec55810bf60",
            "af6002c8e204c93f1bdc844096d8b66be7d60f246f7f352e1ee2e22ec152d81b",
            "fe839998744b4879b720498acdf9463d364e9be428254b96cd112ed1c468eec3de",
            "fc7c7929032069bdc50384d75704fa32d0e830c86e91c160b5f123d50dd48bb84",
            "ba74fcfa4e70a6e4d0f885a6a588b3f1608d02642794cea4bca920c634f5de1ed",
            "4bd3956039f15a6d78ae2657efa02c7d84a2dd0b9fb8c4db4f554fa4c9ea9a76c",
            "6550082e96a8eb1c8118b4f472fb4eacb1d5e6fb5601df483d86b984ec2c73570",
            "16896591b5daff871096f5f6224dcd2e0a825b3077a5cc34c9d1ab0"
        ));

        let result = srp_client_compute_with_private(
            &salt,
            &server_b,
            SRP_TRANSIENT_PIN,
            &private_a,
        ).unwrap();

        assert_eq!(
            hex(&result.session_key),
            "0bb51ee9d1d0b34698d1c38b5e3060b81f514a2d5cf8f1286d3b40fa2750f5c3d718a36ccbc8280b8b164b7d616d63e1c52f1b418fbe9eb95f4b93b6b3552960"
        );
        assert_eq!(
            hex(&result.proof_m1),
            "2a323c6cb5ff6c463214d5a185fe33e747fcd80081e14bdddef3ee0ef6b863edf72d2d5d9c80e76170d762c55cf918954865196880c273c44fbc1b3c35f1cd1c"
        );
        assert_eq!(
            hex(&result.expected_hamk),
            "a2a6f0577643afbe744125e4de749d4def30641050f15460733f4fe8666d219c0636562704c33d04ed526682e91632f87f83fcd573f7e51a57691de3b43c8a59"
        );
    }

    #[test]
    fn invalid_zero_server_key_is_rejected() {
        assert_eq!(
            srp_client_compute_with_private(
                &[0u8; 16],
                &[0u8],
                SRP_TRANSIENT_PIN,
                &[1u8; 32],
            ),
            Err(SrpError::InvalidServerPublicKey)
        );
    }

    #[test]
    fn runtime_private_value_produces_expected_sizes() {
        let salt = [0x11u8; 16];
        let n = modulus();
        let g = BigUint::from(5u8);
        let x_inner = sha512(b"Pair-Setup:3939");
        let mut x_input = Vec::from(salt);
        x_input.extend_from_slice(&x_inner);
        let x = BigUint::from_bytes_be(&sha512(&x_input));
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&sha512(&[pad(&n), pad(&g)].concat()));
        let b = BigUint::from(7u8);
        let server_b = ((&k * &v) + g.modpow(&b, &n)) % &n;

        let result = srp_client_compute(&salt, &minimal_bytes(&server_b), SRP_TRANSIENT_PIN).unwrap();
        assert!(!result.public_key_a.is_empty());
        assert_eq!(result.proof_m1.len(), 64);
        assert_eq!(result.expected_hamk.len(), 64);
        assert_eq!(result.session_key.len(), 64);
    }
}
