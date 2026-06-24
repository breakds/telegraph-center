//! Monitor route handlers.
//!
//! Handlers orchestrate; the password, session, throttle, cookie, and CSRF
//! algorithms live in sibling modules. Every error path renders a generic
//! message so the response never reveals whether the username exists, nor leaks
//! configuration or storage details.

use askama::Template;
use axum::extract::{Form, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::storage::NewLoginFailure;

use super::auth::AuthenticatedSession;
use super::templates::{LoginTemplate, MonitorTemplate};
use super::throttle::{LoginThrottle, ThrottleDecision};
use super::{MonitorState, THROTTLE_WINDOW, cookies, csrf};

const LOGIN_PATH: &str = "/monitor/login";
const MONITOR_PATH: &str = "/monitor";

/// Remote IP used for throttling. Until nginx normalizes a trusted client-IP
/// header (M8), we do not trust arbitrary request headers, so failures are
/// counted per username against a single conservative bucket.
const REMOTE_IP: &str = "unknown";

/// Form body for `POST /monitor/login`.
#[derive(Debug, Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// Form body for state-changing POSTs that carry only a CSRF token.
#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

/// `GET /monitor/login`: render the login form, or redirect an already
/// authenticated Operator to the monitor.
pub async fn login_get(State(state): State<MonitorState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookies::read_session_cookie(&headers) {
        let now = state.clock.now();
        if let Ok(Some(_)) = state.auth.authenticate(&state.store, &token, now).await {
            return Redirect::to(MONITOR_PATH).into_response();
        }
    }
    render_login("", StatusCode::OK)
}

/// `POST /monitor/login`: throttle, verify credentials, and on success create a
/// session and set the cookie.
pub async fn login_post(
    State(state): State<MonitorState>,
    Form(form): Form<LoginForm>,
) -> Response {
    let now = state.clock.now();

    let count = match state
        .store
        .count_login_failures_since(&form.username, REMOTE_IP, now - THROTTLE_WINDOW)
        .await
    {
        Ok(count) => count,
        Err(error) => return internal_error(&format!("count login failures: {error}")),
    };

    match LoginThrottle::decision(count) {
        ThrottleDecision::Locked => {
            return render_login(
                "Too many attempts. Please try again later.",
                StatusCode::TOO_MANY_REQUESTS,
            );
        }
        ThrottleDecision::Delay(delay) => state.sleeper.sleep(delay.unsigned_abs()).await,
        ThrottleDecision::Allow => {}
    }

    match state
        .auth
        .verify_credentials(&form.username, &form.password)
    {
        Ok(true) => finish_login(&state, &form.username, now).await,
        Ok(false) => {
            record_failure(&state, &form.username, now).await;
            render_login("Invalid username or password.", StatusCode::UNAUTHORIZED)
        }
        Err(error) => internal_error(&format!("verify credentials: {error}")),
    }
}

/// `POST /monitor/logout`: require a valid session and CSRF token, then revoke
/// the session and clear the cookie.
pub async fn logout_post(
    State(state): State<MonitorState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let auth = match require_session(&state, &headers).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };

    if !csrf::validate(&auth.session, &form.csrf_token) {
        return (StatusCode::FORBIDDEN, "Invalid CSRF token.").into_response();
    }

    let now = state.clock.now();
    if let Err(error) = state
        .store
        .revoke_session(&auth.session.session_hash, now)
        .await
    {
        return internal_error(&format!("revoke session: {error}"));
    }

    let mut response = Redirect::to(LOGIN_PATH).into_response();
    set_cookie(&mut response, cookies::clear_session_cookie());
    response
}

/// `GET /monitor`: the authenticated placeholder page.
pub async fn monitor_index(State(state): State<MonitorState>, headers: HeaderMap) -> Response {
    let auth = match require_session(&state, &headers).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    render(
        MonitorTemplate {
            username: auth.session.operator_username.clone(),
            csrf_token: auth.csrf_token,
        },
        StatusCode::OK,
    )
}

/// Resolve the current session from the cookie, or return a redirect to login.
async fn require_session(
    state: &MonitorState,
    headers: &HeaderMap,
) -> Result<AuthenticatedSession, Response> {
    let token = cookies::read_session_cookie(headers).ok_or_else(redirect_to_login)?;
    let now = state.clock.now();
    match state.auth.authenticate(&state.store, &token, now).await {
        Ok(Some(auth)) => Ok(auth),
        Ok(None) => Err(redirect_to_login()),
        Err(error) => Err(internal_error(&format!("authenticate: {error}"))),
    }
}

/// Create the session, clear prior failures, and respond with the cookie.
async fn finish_login(state: &MonitorState, username: &str, now: OffsetDateTime) -> Response {
    let started = match state.auth.start_session(&state.store, now).await {
        Ok(started) => started,
        Err(error) => return internal_error(&format!("start session: {error}")),
    };
    if let Err(error) = state.store.clear_login_failures(username, REMOTE_IP).await {
        return internal_error(&format!("clear login failures: {error}"));
    }

    let mut response = Redirect::to(MONITOR_PATH).into_response();
    set_cookie(
        &mut response,
        cookies::set_session_cookie(&started.session_token),
    );
    response
}

/// Record a failed login attempt. A storage failure here is logged but does not
/// change the generic response shown to the user.
async fn record_failure(state: &MonitorState, username: &str, now: OffsetDateTime) {
    let failure = NewLoginFailure {
        id: state.ids.generate(),
        username: username.to_string(),
        remote_ip: REMOTE_IP.to_string(),
        failed_at: now,
    };
    if let Err(error) = state.store.record_login_failure(failure).await {
        eprintln!("monitor: failed to record login failure: {error}");
    }
}

fn redirect_to_login() -> Response {
    Redirect::to(LOGIN_PATH).into_response()
}

fn render_login(error: &str, status: StatusCode) -> Response {
    render(
        LoginTemplate {
            error: error.to_string(),
        },
        status,
    )
}

fn render<T: Template>(template: T, status: StatusCode) -> Response {
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(error) => internal_error(&format!("render template: {error}")),
    }
}

/// A generic 500 that logs the cause server-side without leaking it.
fn internal_error(detail: &str) -> Response {
    eprintln!("monitor internal error: {detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "An unexpected error occurred.",
    )
        .into_response()
}

fn set_cookie(response: &mut Response, cookie: String) {
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
}
