//! The single monitor session cookie: centralized attributes and parsing.
//!
//! Per ADR 0006 the cookie is scoped to `/monitor` so it is never sent to
//! `/api/*`. It is `HttpOnly` (script cannot read it), `Secure` (HTTPS only),
//! and `SameSite=Lax` (not sent on cross-site POSTs). The raw session token is
//! the only value carried.

use axum::http::HeaderMap;
use axum::http::header::COOKIE;

/// Name of the monitor session cookie.
pub const SESSION_COOKIE: &str = "telegraph_operator_session";

/// Path the cookie is scoped to, matching the monitor mount point.
pub const COOKIE_PATH: &str = "/monitor";

/// Cookie lifetime, aligned with the absolute session lifetime (14 days).
const MAX_AGE_SECONDS: i64 = 14 * 24 * 60 * 60;

/// Build the `Set-Cookie` value that establishes a session.
pub fn set_session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path={COOKIE_PATH}; Max-Age={MAX_AGE_SECONDS}; HttpOnly; Secure; SameSite=Lax"
    )
}

/// Build the `Set-Cookie` value that clears the session (same path, expired).
pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path={COOKIE_PATH}; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Read the session token from the request's `Cookie` header, if present and
/// non-empty.
pub fn read_session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let (name, value) = pair.trim().split_once('=')?;
        if name.trim() == SESSION_COOKIE {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_cookie_carries_the_required_attributes() {
        let cookie = set_session_cookie("abc123");
        assert!(cookie.starts_with("telegraph_operator_session=abc123;"));
        assert!(cookie.contains("Path=/monitor"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=1209600"));
    }

    #[test]
    fn clear_cookie_expires_immediately_on_the_same_path() {
        let cookie = clear_session_cookie();
        assert!(cookie.contains("Path=/monitor"));
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn reads_the_session_token_among_other_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            "other=1; telegraph_operator_session=tok; theme=dark"
                .parse()
                .unwrap(),
        );
        assert_eq!(read_session_cookie(&headers).as_deref(), Some("tok"));
    }

    #[test]
    fn missing_or_empty_cookie_is_none() {
        let empty = HeaderMap::new();
        assert_eq!(read_session_cookie(&empty), None);

        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "telegraph_operator_session=".parse().unwrap());
        assert_eq!(read_session_cookie(&headers), None);
    }
}
