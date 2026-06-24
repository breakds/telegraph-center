//! Operator password verification and session lifecycle.
//!
//! Password verification is an Argon2id PHC check against a hash supplied by a
//! [`PasswordHashProvider`] (the environment in production). Session creation,
//! validation, and revocation translate raw tokens to stored hashes and apply
//! the [`super::session`] validity rules; the raw password, hash, and tokens are
//! never logged.

use std::sync::Arc;

use argon2::password_hash::Error as PasswordHashError;
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use time::OffsetDateTime;

use crate::config::OperatorConfig;
use crate::domain::OperatorSession;
use crate::storage::{NewOperatorSession, SqliteStore, StorageError};

use super::session::{self, SessionValidity};
use super::tokens;
use super::{ABSOLUTE_TTL, IDLE_TTL};

/// A reason an authentication operation failed.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The configured password-hash environment variable is not set.
    #[error("password hash environment variable {0} is not set")]
    MissingPasswordHash(String),
    /// The configured password-hash environment variable is empty.
    #[error("password hash environment variable {0} is empty")]
    BlankPasswordHash(String),
    /// The stored password hash is not a valid Argon2id PHC string.
    #[error("stored password hash is not a valid argon2 PHC string")]
    MalformedPasswordHash,
    /// An underlying storage failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Verify a password against an Argon2id PHC hash string.
///
/// Returns `Ok(true)` on a match and `Ok(false)` on a wrong password. A hash
/// that cannot be parsed or verified for any other reason is an error, never a
/// silent success.
pub fn verify_password(phc_hash: &str, password: &str) -> Result<bool, AuthError> {
    let parsed = PasswordHash::new(phc_hash).map_err(|_| AuthError::MalformedPasswordHash)?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(PasswordHashError::Password) => Ok(false),
        Err(_) => Err(AuthError::MalformedPasswordHash),
    }
}

/// Source of the Operator's Argon2id PHC password hash.
///
/// The hash is a secret, so it is loaded on demand rather than baked into
/// config. Production reads it from the environment; tests can supply a fixed
/// value without touching process-global state.
pub trait PasswordHashProvider: Send + Sync {
    /// Load the current PHC hash, or report why it is unavailable.
    fn load(&self) -> Result<String, AuthError>;
}

/// Reads the PHC hash from a named environment variable at verification time.
pub struct EnvPasswordHash {
    env_name: String,
}

impl EnvPasswordHash {
    /// Build a provider that reads from `env_name`.
    pub fn new(env_name: impl Into<String>) -> Self {
        Self {
            env_name: env_name.into(),
        }
    }
}

impl PasswordHashProvider for EnvPasswordHash {
    fn load(&self) -> Result<String, AuthError> {
        let value = std::env::var(&self.env_name)
            .map_err(|_| AuthError::MissingPasswordHash(self.env_name.clone()))?;
        if value.trim().is_empty() {
            return Err(AuthError::BlankPasswordHash(self.env_name.clone()));
        }
        Ok(value)
    }
}

/// A provider over an in-memory PHC hash, for tests and non-environment secret
/// sources.
pub struct StaticPasswordHash(pub String);

impl PasswordHashProvider for StaticPasswordHash {
    fn load(&self) -> Result<String, AuthError> {
        if self.0.trim().is_empty() {
            return Err(AuthError::BlankPasswordHash("<static>".to_string()));
        }
        Ok(self.0.clone())
    }
}

/// A freshly created session: the raw tokens to hand to the client. Only their
/// hashes are stored.
pub struct StartedSession {
    /// Raw session token for the cookie.
    pub session_token: String,
    /// Raw CSRF token for forms.
    pub csrf_token: String,
}

/// A validated, refreshed session plus its derived raw CSRF token (for
/// rendering forms on the authenticated page).
pub struct AuthenticatedSession {
    /// The session after its idle window was refreshed.
    pub session: OperatorSession,
    /// The raw CSRF token derived from the presented session token.
    pub csrf_token: String,
}

/// Verifies credentials and manages Operator Sessions for the single configured
/// Operator account.
pub struct AuthService {
    username: String,
    hash_provider: Arc<dyn PasswordHashProvider>,
}

impl AuthService {
    /// Build an auth service for `username` whose password hash comes from
    /// `hash_provider`.
    pub fn new(username: impl Into<String>, hash_provider: Arc<dyn PasswordHashProvider>) -> Self {
        Self {
            username: username.into(),
            hash_provider,
        }
    }

    /// Build the production auth service, reading the password hash from the
    /// environment variable named in config.
    pub fn from_config(operator: &OperatorConfig) -> Self {
        Self::new(
            operator.username.clone(),
            Arc::new(EnvPasswordHash::new(operator.password_hash_env.clone())),
        )
    }

    /// Verify a submitted username/password pair.
    ///
    /// A mismatched username yields `Ok(false)` without consulting the hash. The
    /// caller is responsible for showing a single generic message so neither
    /// branch reveals whether the username was valid.
    pub fn verify_credentials(&self, username: &str, password: &str) -> Result<bool, AuthError> {
        if username != self.username {
            return Ok(false);
        }
        let phc = self.hash_provider.load()?;
        verify_password(&phc, password)
    }

    /// Create a new Operator Session, persisting only token hashes and returning
    /// the raw tokens for the cookie and forms.
    pub async fn start_session(
        &self,
        store: &SqliteStore,
        now: OffsetDateTime,
    ) -> Result<StartedSession, AuthError> {
        let session_token = tokens::generate_token();
        let csrf_token = tokens::derive_csrf(&session_token);
        store
            .create_session(NewOperatorSession {
                session_hash: tokens::hash_token(&session_token),
                operator_username: self.username.clone(),
                csrf_token_hash: tokens::hash_token(&csrf_token),
                created_at: now,
                last_seen_at: now,
                idle_expires_at: now + IDLE_TTL,
                absolute_expires_at: now + ABSOLUTE_TTL,
            })
            .await?;
        Ok(StartedSession {
            session_token,
            csrf_token,
        })
    }

    /// Validate a presented session token. On success the session's idle window
    /// is refreshed (capped at the absolute deadline) and the session plus its
    /// derived CSRF token is returned. An unknown, revoked, or expired token
    /// yields `Ok(None)`.
    pub async fn authenticate(
        &self,
        store: &SqliteStore,
        session_token: &str,
        now: OffsetDateTime,
    ) -> Result<Option<AuthenticatedSession>, AuthError> {
        let session_hash = tokens::hash_token(session_token);
        let Some(mut session) = store.get_session(&session_hash).await? else {
            return Ok(None);
        };
        if session::evaluate_session(&session, now) != SessionValidity::Active {
            return Ok(None);
        }

        // The refresh is also the authority on liveness: `touch_session` only
        // updates rows that are still un-revoked, so a `false` here means the
        // session was revoked between the read above and now. Treat that as
        // invalid rather than letting a just-logged-out session proceed.
        let refreshed_idle = (now + IDLE_TTL).min(session.absolute_expires_at);
        if !store
            .touch_session(&session_hash, now, refreshed_idle)
            .await?
        {
            return Ok(None);
        }
        session.last_seen_at = now;
        session.idle_expires_at = refreshed_idle;

        Ok(Some(AuthenticatedSession {
            csrf_token: tokens::derive_csrf(session_token),
            session,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::{PasswordHasher, SaltString};

    /// Mint an Argon2id PHC hash for `password` with a fixed salt, so tests stay
    /// deterministic and need no RNG feature.
    fn fixture_hash(password: &str) -> String {
        let salt = SaltString::encode_b64(b"telegraph-fixed-salt").unwrap();
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    #[test]
    fn verifies_correct_password_and_rejects_wrong() {
        let hash = fixture_hash("correct horse");
        assert!(verify_password(&hash, "correct horse").unwrap());
        assert!(!verify_password(&hash, "wrong").unwrap());
    }

    #[test]
    fn malformed_hash_is_an_error_not_a_match() {
        assert!(matches!(
            verify_password("not-a-phc-hash", "whatever"),
            Err(AuthError::MalformedPasswordHash)
        ));
    }

    #[test]
    fn env_provider_reports_missing_and_blank() {
        let missing = EnvPasswordHash::new("TELEGRAPH_TEST_HASH_DEFINITELY_UNSET");
        assert!(matches!(
            missing.load(),
            Err(AuthError::MissingPasswordHash(_))
        ));

        let name = "TELEGRAPH_TEST_HASH_BLANK_VAR";
        // SAFETY: a uniquely named variable touched only by this test.
        unsafe { std::env::set_var(name, "   ") };
        let blank = EnvPasswordHash::new(name);
        assert!(matches!(blank.load(), Err(AuthError::BlankPasswordHash(_))));
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn verify_credentials_checks_username_then_password() {
        let service = AuthService::new(
            "break",
            Arc::new(StaticPasswordHash(fixture_hash("secret"))),
        );
        assert!(service.verify_credentials("break", "secret").unwrap());
        assert!(!service.verify_credentials("break", "nope").unwrap());
        // Wrong username never consults the hash.
        assert!(!service.verify_credentials("intruder", "secret").unwrap());
    }

    #[tokio::test]
    async fn authenticate_denies_a_revoked_session() {
        use crate::storage::SqliteStore;
        use time::macros::datetime;

        let dir = tempfile::TempDir::new().unwrap();
        let store = SqliteStore::connect(dir.path().join("t.db")).await.unwrap();
        let service = AuthService::new(
            "break",
            Arc::new(StaticPasswordHash(fixture_hash("secret"))),
        );
        let now = datetime!(2026-06-24 12:00:00 UTC);

        let started = service.start_session(&store, now).await.unwrap();
        // Active session authenticates and refreshes its idle window.
        assert!(
            service
                .authenticate(&store, &started.session_token, now)
                .await
                .unwrap()
                .is_some()
        );

        // Once revoked, the same token must be denied. `authenticate` honors the
        // un-revoked guard in `touch_session`, so a session revoked concurrently
        // with a request never proceeds.
        let hash = tokens::hash_token(&started.session_token);
        assert!(store.revoke_session(&hash, now).await.unwrap());
        assert!(
            service
                .authenticate(&store, &started.session_token, now)
                .await
                .unwrap()
                .is_none()
        );
    }
}
