//! Session and CSRF token generation and hashing.
//!
//! Tokens are high-entropy random values from the OS CSPRNG. Only their SHA-256
//! hashes are persisted, so a database read never exposes a usable token. The
//! per-session CSRF token is derived from the session token so it can be
//! recomputed on each authenticated page render without a second cookie or a
//! stored raw value.

use sha2::{Digest, Sha256};

/// Number of random bytes in a session token (256 bits of entropy).
const TOKEN_BYTES: usize = 32;

/// Domain separator mixed in when deriving a CSRF token from a session token, so
/// the two values can never collide.
const CSRF_DOMAIN: &[u8] = b":telegraph-csrf-v1";

/// Generate a fresh random token as lowercase hex.
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG must be available");
    to_hex(&bytes)
}

/// Hash a token for storage or comparison. Tokens are high-entropy, so a fast
/// cryptographic hash (not a password KDF) is the right tool here.
pub fn hash_token(token: &str) -> String {
    to_hex(&Sha256::digest(token.as_bytes()))
}

/// Derive a session's CSRF token from its (secret) session token. Knowing the
/// session token is required to compute it, so an attacker who cannot read the
/// `HttpOnly` cookie cannot forge a valid CSRF token.
pub fn derive_csrf(session_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_token.as_bytes());
    hasher.update(CSRF_DOMAIN);
    to_hex(&hasher.finalize())
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
    fn generated_tokens_are_64_hex_chars_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b, "two generated tokens must differ");
    }

    #[test]
    fn hashing_does_not_reveal_the_raw_token() {
        let token = "0123456789abcdef";
        let hash = hash_token(token);
        assert_ne!(hash, token);
        assert_eq!(hash.len(), 64);
        // Hashing is deterministic so it can validate a presented token.
        assert_eq!(hash, hash_token(token));
    }

    #[test]
    fn csrf_is_bound_to_the_session_token() {
        let csrf = derive_csrf("session-token");
        assert_eq!(csrf, derive_csrf("session-token"));
        assert_ne!(csrf, derive_csrf("other-token"));
        // The CSRF token is not just the session token rehashed plainly.
        assert_ne!(csrf, hash_token("session-token"));
    }
}
