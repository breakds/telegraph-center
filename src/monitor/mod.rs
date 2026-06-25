//! Operator authentication and the `/monitor` surface.
//!
//! The monitor is app-managed (ADR 0005): the app verifies the Operator
//! password, issues SQLite-backed Operator Sessions, throttles login, and
//! protects state-changing POSTs with per-session CSRF tokens. nginx still owns
//! TLS and exposure (ADR 0006); cookies are scoped to `/monitor` so they never
//! reach `/api/*`.
//!
//! The pieces are split so the algorithms stay testable without HTTP:
//! [`auth`] (password + session lifecycle), [`session`] (validity rules),
//! [`throttle`] (lockout policy), [`tokens`], [`csrf`], and [`cookies`].

pub mod auth;
pub mod cookies;
pub mod csrf;
pub mod session;
pub mod templates;
pub mod throttle;
pub mod tokens;

mod handlers;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::Router;
use axum::routing::{get, post};
use time::Duration;

use crate::config::AppConfig;
use crate::seam::{Clock, IdGenerator};
use crate::storage::SqliteStore;

pub use auth::{AuthError, AuthService, EnvPasswordHash, PasswordHashProvider, verify_password};
pub use session::{SessionValidity, evaluate_session};
pub use throttle::{LoginThrottle, ThrottleDecision};

/// Idle session timeout: a session unused for this long becomes invalid.
pub const IDLE_TTL: Duration = Duration::days(7);

/// Absolute session lifetime: a session older than this is invalid regardless of
/// activity.
pub const ABSOLUTE_TTL: Duration = Duration::days(14);

/// Window over which login failures are counted for throttling.
pub const THROTTLE_WINDOW: Duration = Duration::minutes(15);

/// Async sleep seam, so login-throttling delays are real in production but
/// instant and observable in tests.
#[allow(async_fn_in_trait)]
pub trait Sleeper: Send + Sync {
    /// Sleep for `duration`.
    fn sleep(&self, duration: StdDuration) -> futures_sleep::BoxSleep<'_>;
}

/// Boxed-future helper so [`Sleeper`] stays object-safe behind `Arc<dyn ...>`.
pub mod futures_sleep {
    use std::future::Future;
    use std::pin::Pin;

    /// A boxed, `Send` future returned by [`super::Sleeper::sleep`].
    pub type BoxSleep<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Production sleeper backed by Tokio's timer.
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, duration: StdDuration) -> futures_sleep::BoxSleep<'_> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Shared state for the monitor routes. Cheap to clone.
#[derive(Clone)]
pub struct MonitorState {
    /// Validated application configuration (configured Sinks for Manual Routing).
    pub config: Arc<AppConfig>,
    /// SQLite-backed repository (sessions, login failures, Recordings).
    pub store: SqliteStore,
    /// Clock seam.
    pub clock: Arc<dyn Clock>,
    /// Identifier generator (login-failure, Delivery, and audit ids).
    pub ids: Arc<dyn IdGenerator>,
    /// Operator credential and session service.
    pub auth: Arc<AuthService>,
    /// Sleep seam for throttling delays.
    pub sleeper: Arc<dyn Sleeper>,
}

/// Build the monitor router.
pub fn router(state: MonitorState) -> Router {
    Router::new()
        .route("/monitor", get(handlers::recording_list))
        .route("/monitor/recordings/{id}", get(handlers::recording_detail))
        .route(
            "/monitor/recordings/{id}/manual-route",
            post(handlers::manual_route),
        )
        .route(
            "/monitor/recordings/{id}/retry-transcription",
            post(handlers::retry_transcription),
        )
        .route(
            "/monitor/recordings/{id}/retry-delivery",
            post(handlers::retry_delivery),
        )
        .route(
            "/monitor/login",
            get(handlers::login_get).post(handlers::login_post),
        )
        .route("/monitor/logout", post(handlers::logout_post))
        .with_state(state)
}
