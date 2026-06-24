//! HMAC-SHA256 signing of webhook bodies.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Sign `body` with `secret` and return the lowercase hex HMAC-SHA256 digest.
///
/// Signs the exact bytes passed in, which must be the same bytes sent over HTTP.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(body);
    to_hex(&mac.finalize().into_bytes())
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_vector() {
        // Standard HMAC-SHA256 test vector.
        let signature = sign(b"key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            signature,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn output_is_lowercase_hex_of_expected_length() {
        let signature = sign(b"secret", b"{}");
        assert_eq!(signature.len(), 64);
        assert!(
            signature
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
