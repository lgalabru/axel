//! Minimal lowercase-hex helpers.
//!
//! Kept here (rather than pulling in the `hex` crate) so `axel-inbox` and
//! `axel-identity` encode ids and signatures identically.

/// Encode bytes as lowercase hex.
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// Decode a hex string (either case) into bytes.
///
/// Returns `None` for odd length or non-hex characters.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let (pairs, _) = bytes.as_chunks::<2>();
    for &[hi, lo] in pairs {
        let hi = (hi as char).to_digit(16)?;
        let lo = (lo as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Decode exactly `N` bytes of hex, rejecting any other length.
pub fn decode_array<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let v = decode(s)?;
    let mut out = [0u8; N];
    out.copy_from_slice(&v);
    Some(out)
}

/// True when every character is a lowercase hex digit (`0-9a-f`).
pub fn is_lowercase_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let bytes = [0u8, 1, 0xab, 0xff, 0x10];
        let s = encode(&bytes);
        assert_eq!(s, "0001abff10");
        assert_eq!(decode(&s).unwrap(), bytes);
        assert_eq!(decode("0001ABFF10").unwrap(), bytes);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(decode("abc").is_none());
        assert!(decode("zz").is_none());
        assert!(decode("ééé").is_none());
        assert!(decode_array::<2>("abcdef").is_none());
        assert_eq!(decode_array::<2>("abcd"), Some([0xab, 0xcd]));
    }

    #[test]
    fn lowercase_check() {
        assert!(is_lowercase_hex("0123456789abcdef"));
        assert!(!is_lowercase_hex("ABCDEF"));
        assert!(!is_lowercase_hex("0g"));
        assert!(is_lowercase_hex(""));
    }
}
