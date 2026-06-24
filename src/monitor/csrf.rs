//! CSRF token validation for state-changing monitor POSTs.
//!
//! Forms carry the raw per-session CSRF token in a hidden field. A POST is
//! accepted only when the hash of the submitted token matches the session's
//! stored `csrf_token_hash`. A failure is reported to the caller as a `403`; it
//! never revokes the session.

use crate::domain::OperatorSession;

use super::tokens;

/// Whether `submitted` is the valid CSRF token for `session`.
pub fn validate(session: &OperatorSession, submitted: &str) -> bool {
    let submitted = submitted.trim();
    if submitted.is_empty() {
        return false;
    }
    constant_time_eq(
        tokens::hash_token(submitted).as_bytes(),
        session.csrf_token_hash.as_bytes(),
    )
}

/// Compare two byte slices without short-circuiting on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn session_for(csrf_token: &str) -> OperatorSession {
        OperatorSession {
            session_hash: "sess".into(),
            operator_username: "break".into(),
            csrf_token_hash: tokens::hash_token(csrf_token),
            created_at: datetime!(2026-06-24 12:00:00 UTC),
            last_seen_at: datetime!(2026-06-24 12:00:00 UTC),
            idle_expires_at: datetime!(2026-07-01 12:00:00 UTC),
            absolute_expires_at: datetime!(2026-07-08 12:00:00 UTC),
            revoked_at: None,
        }
    }

    #[test]
    fn accepts_the_matching_token() {
        let session = session_for("the-csrf-token");
        assert!(validate(&session, "the-csrf-token"));
    }

    #[test]
    fn rejects_wrong_empty_or_blank_tokens() {
        let session = session_for("the-csrf-token");
        assert!(!validate(&session, "wrong-token"));
        assert!(!validate(&session, ""));
        assert!(!validate(&session, "   "));
    }
}
