//! Ranger's public request identifier, generated from this installation's data.
//!
//! Evidence: ARM64 SetEnv at 0x48f1f4..0x48f3a4 parses Env.android_id and
//! Env.user_id, then calls 0x6b8dc8. That generator formats 42 lowercase hex
//! bytes, checksums them with MurmurHash3 x86_32 seed 42 (0x56b518), packs a
//! 24-byte record and encodes it with its rotated alphabet (0x6bd250).
//! Six synthetic executions of the original generator are recorded in
//! analysis/playback/ranger-identity-vectors.synthetic.json. The authorized
//! capture check separately verified the format without exporting real IDs.
//!
//! Windows adaptation: enrollment already sends the first 16 hex characters
//! of its own persistent installation_id as androidId. Use exactly those
//! bytes here, never an Android device ID or another installation's header.

const ALPHABET: &[u8; 64] =
    b"B8oOvNYtn9RPijWXbcJqGyD5Eklmh21V_efAgLC3wxF4STpQr6usdHM-Kz0IaUZ7";

/// Build a Ranger-Id for the current installation and server-issued user ID.
///
/// The caller supplies its own nonce and epoch clock and retains the result for
/// the current Env/session, matching generation during native SetEnv. Native
/// user conversion is atoll followed by a 32-bit register move; valid positive
/// decimal i64 inputs therefore retain their low 32 bits. The epoch seconds
/// are likewise passed in a 32-bit register. Values outside atoll's defined
/// positive range are rejected instead of reproducing overflow behavior.
pub fn generate(
    user_id: &str,
    installation_id: &str,
    epoch_seconds: u64,
    nonce: u32,
) -> Result<String, String> {
    if user_id.is_empty() || !user_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("La identidad de la sesión tiene un formato inválido.".into());
    }
    let user = user_id
        .parse::<i64>()
        .map_err(|_| "La identidad de la sesión está fuera del formato admitido.".to_owned())?
        as u32;

    let installation = installation_id.as_bytes();
    if installation.len() != 32 || !installation.iter().all(u8::is_ascii_hexdigit) {
        return Err("La identidad local de la instalación tiene un formato inválido.".into());
    }
    let mut identity_bytes = [0u8; 8];
    for (index, pair) in installation[..16].chunks_exact(2).enumerate() {
        identity_bytes[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    // The original hex decoder writes bytes in string order; its following
    // AArch64 ldr x22 interprets the first eight bytes as little endian.
    let identity = u64::from_le_bytes(identity_bytes);
    let seconds = epoch_seconds as u32;
    let marker = 0x40 | (nonce & 63) as u8;
    let checksum_input = format!("{marker:02x}{user:08x}{identity:016x}{seconds:08x}{nonce:08x}");
    let checksum = murmur32(checksum_input.as_bytes(), 42);

    // 0x6b9420..0x6b94d8: each numeric field is written most-significant
    // byte first. The checksum occupies bytes 0..4, not the final four bytes.
    let mut record = [0u8; 24];
    record[0..4].copy_from_slice(&checksum.to_be_bytes());
    record[4..8].copy_from_slice(&user.to_be_bytes());
    record[8..16].copy_from_slice(&identity.to_be_bytes());
    record[16..20].copy_from_slice(&seconds.to_be_bytes());
    record[20..24].copy_from_slice(&nonce.to_be_bytes());

    let rotation = (nonce & 63) as usize;
    let mut result = format!("{marker:02x}");
    result.reserve(32);
    for bytes in record.chunks_exact(3) {
        let word = (u32::from(bytes[0]) << 16)
            | (u32::from(bytes[1]) << 8)
            | u32::from(bytes[2]);
        for shift in [18, 12, 6, 0] {
            let index = ((word >> shift) & 63) as usize;
            result.push(ALPHABET[(index + rotation) & 63] as char);
        }
    }
    Ok(result)
}

// Only called after validating all installation bytes as ASCII hex.
fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!("validated hexadecimal byte"),
    }
}

fn murmur32(data: &[u8], seed: u32) -> u32 {
    let mut hash = seed;
    let mut blocks = data.chunks_exact(4);
    for bytes in &mut blocks {
        let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            .wrapping_mul(0xcc9e_2d51)
            .rotate_left(15)
            .wrapping_mul(0x1b87_3593);
        hash ^= word;
        hash = hash.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let tail = blocks.remainder();
    if !tail.is_empty() {
        let mut word = 0u32;
        for (index, byte) in tail.iter().enumerate() {
            word |= u32::from(*byte) << (index * 8);
        }
        hash ^= word
            .wrapping_mul(0xcc9e_2d51)
            .rotate_left(15)
            .wrapping_mul(0x1b87_3593);
    }
    hash ^= data.len() as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^ (hash >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALLATION: &str = "00112233445566778899aabbccddeeff";

    // This decoder is test-only. A request header from outside the current
    // installation must never become an input to production identity state.
    fn decode_checked(value: &str) -> Option<[u8; 24]> {
        if value.len() != 34 || !value.is_ascii() {
            return None;
        }
        let marker = u8::from_str_radix(&value[..2], 16).ok()?;
        if !(0x40..=0x7f).contains(&marker) {
            return None;
        }
        let rotation = (marker & 63) as usize;
        let mut record = [0u8; 24];
        for (index, group) in value.as_bytes()[2..].chunks_exact(4).enumerate() {
            let mut word = 0u32;
            for byte in group {
                let position = ALPHABET.iter().position(|symbol| symbol == byte)?;
                word = (word << 6) | ((position + 64 - rotation) & 63) as u32;
            }
            record[index * 3..index * 3 + 3].copy_from_slice(&word.to_be_bytes()[1..]);
        }
        let checksum = u32::from_be_bytes(record[0..4].try_into().ok()?);
        let user = u32::from_be_bytes(record[4..8].try_into().ok()?);
        let identity = u64::from_be_bytes(record[8..16].try_into().ok()?);
        let seconds = u32::from_be_bytes(record[16..20].try_into().ok()?);
        let nonce = u32::from_be_bytes(record[20..24].try_into().ok()?);
        let text = format!("{marker:02x}{user:08x}{identity:016x}{seconds:08x}{nonce:08x}");
        if marker != 0x40 | (nonce & 63) as u8 || checksum != murmur32(text.as_bytes(), 42) {
            return None;
        }
        Some(record)
    }

    #[test]
    fn matches_original_arm64_synthetic_generator() {
        // Original 0x6b8dc8 results, not generated by this Rust implementation.
        let cases = [
            ("0", "00000000000000000000000000000000", 0, 0,
             "40CEPqCBBBBBBBBBBBBBBBBBBBBBBBBBBB"),
            ("1", "0123456789abcdeffedcba9876543210", 1_700_000_000, 1,
             "41A6EFB68888nrdmQRlHyg85DGUc88888o"),
            ("305419896", INSTALLATION, 0x1234_5678, 0x1234_567f,
             "7f7hXop_tsGC_MEgGOP6tb7BtsGCVcivEZ"),
            ("4294967295", "ffffffffffffffff0000000000000000", 0xffff_ffff, 0xffff_ffff,
             "7fpcJx3pZZZZZZZZZZZZZZZZZZZZZZZZZZ"),
            ("4294967297", "deadbeefcafebabe0011223344556677", 0x1_0000_0002, 0x8000_0000,
             "40pYRP7bBBBBYZpQIRII0T-_BBBBRBBBBB"),
            ("9223372036854775807", "FEDCBA98765432100123456789ABCDEF", 1_700_000_123, 0x7654_322a,
             "6ar7C2VkxxxxC0MXI_bHyYxb7UC85_7TuG"),
        ];
        for (user, installation, seconds, nonce, expected) in cases {
            let actual = generate(user, installation, seconds, nonce).unwrap();
            assert_eq!(actual, expected, "synthetic original-generator fixture");
            assert!(decode_checked(&actual).is_some());
        }
    }

    #[test]
    fn checksum_is_first_and_fields_have_native_byte_order() {
        let record = decode_checked("7f7hXop_tsGC_MEgGOP6tb7BtsGCVcivEZ").unwrap();
        assert_eq!(record, [
            0x01, 0xd4, 0x03, 0xbe, // checksum, big endian
            0x12, 0x34, 0x56, 0x78, // low 32 bits of numeric user ID
            0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, // LE hex input, BE field
            0x12, 0x34, 0x56, 0x78, // epoch seconds, big endian
            0x12, 0x34, 0x56, 0x7f, // nonce, big endian
        ]);
        assert_eq!(murmur32(b"7f123456787766554433221100123456781234567f", 42), 0x01d4_03be);
    }

    #[test]
    fn checksum_detects_changed_payload_or_rotation_marker() {
        let valid = generate("305419896", INSTALLATION, 0x1234_5678, 0x1234_567f).unwrap();
        for index in 2..34 {
            let mut changed = valid.as_bytes().to_vec();
            changed[index] = if changed[index] == b'B' { b'8' } else { b'B' };
            assert!(decode_checked(std::str::from_utf8(&changed).unwrap()).is_none());
        }
        let changed_marker = format!("40{}", &valid[2..]);
        assert!(decode_checked(&changed_marker).is_none());
    }

    #[test]
    fn uses_enrolled_installation_prefix_and_all_nonce_rotations() {
        for nonce in 0..64 {
            let lower = generate("42", INSTALLATION, 1_700_000_000, nonce).unwrap();
            let upper = generate("42", "0011223344556677FFEEDDCCBBAA9988", 1_700_000_000, nonce).unwrap();
            assert_eq!(lower, upper, "only the enrolled prefix enters the header");
            assert_eq!(&lower[..2], format!("{:02x}", 0x40 | nonce));
            assert_eq!(lower.len(), 34);
            assert!(decode_checked(&lower).is_some());
        }
    }

    #[test]
    fn rejects_invalid_identity_without_echoing_input() {
        for user in ["", "-1", "+1", " 1", "1suffix", "１２", "9223372036854775808"] {
            assert!(generate(user, INSTALLATION, 0, 0).is_err());
        }
        for installation in ["", "0011223344556677", "00112233-4455-6677-8899-aabbccddeeff", "00112233445566778899aabbccddeefg", "á0112233445566778899aabbccddeeff"] {
            assert!(generate("1", installation, 0, 0).is_err());
        }
        let error = generate("private-invalid-account", INSTALLATION, 0, 0).unwrap_err();
        assert!(!error.contains("private-invalid-account"));
        assert!(!error.contains(INSTALLATION));
    }
}
