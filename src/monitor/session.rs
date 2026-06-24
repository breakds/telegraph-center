//! Pure Operator Session validity rules.
//!
//! Keeping the decision in a small function with no I/O lets the lifetime rules
//! (revoked, idle timeout, absolute timeout) be unit-tested without a database
//! or HTTP. [`super::auth::AuthService`] applies the rule and, when active,
//! refreshes the session's idle window.

use time::OffsetDateTime;

use crate::domain::OperatorSession;

/// Whether a session may be used, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionValidity {
    /// The session is usable.
    Active,
    /// The session was explicitly revoked (for example, by logout).
    Revoked,
    /// The session passed its idle timeout without activity.
    IdleExpired,
    /// The session passed its absolute maximum lifetime.
    AbsoluteExpired,
}

/// Evaluate a session against the current time. Revocation takes precedence,
/// then the absolute deadline, then the idle deadline.
pub fn evaluate_session(session: &OperatorSession, now: OffsetDateTime) -> SessionValidity {
    if session.revoked_at.is_some() {
        SessionValidity::Revoked
    } else if now >= session.absolute_expires_at {
        SessionValidity::AbsoluteExpired
    } else if now >= session.idle_expires_at {
        SessionValidity::IdleExpired
    } else {
        SessionValidity::Active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-06-24 12:00:00 UTC);

    fn session() -> OperatorSession {
        OperatorSession {
            session_hash: "sess".into(),
            operator_username: "break".into(),
            csrf_token_hash: "csrf".into(),
            created_at: NOW,
            last_seen_at: NOW,
            idle_expires_at: NOW + Duration::days(7),
            absolute_expires_at: NOW + Duration::days(14),
            revoked_at: None,
        }
    }

    #[test]
    fn active_within_both_windows() {
        assert_eq!(
            evaluate_session(&session(), NOW + Duration::days(1)),
            SessionValidity::Active
        );
    }

    #[test]
    fn revoked_is_never_active() {
        let mut s = session();
        s.revoked_at = Some(NOW + Duration::hours(1));
        assert_eq!(
            evaluate_session(&s, NOW + Duration::hours(2)),
            SessionValidity::Revoked
        );
    }

    #[test]
    fn idle_timeout_invalidates() {
        assert_eq!(
            evaluate_session(&session(), NOW + Duration::days(7)),
            SessionValidity::IdleExpired
        );
    }

    #[test]
    fn absolute_timeout_takes_precedence_over_idle() {
        let mut s = session();
        // Push idle out so only the absolute deadline can fire.
        s.idle_expires_at = NOW + Duration::days(30);
        assert_eq!(
            evaluate_session(&s, NOW + Duration::days(14)),
            SessionValidity::AbsoluteExpired
        );
    }
}
