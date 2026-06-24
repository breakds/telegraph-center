//! Monitor authentication tests driven through Tower service calls.
//!
//! No network, browser, or deployment secrets: requests are built directly and
//! sent through `oneshot`. The clock, ids, and sleep are injected for
//! determinism, and the Operator password hash comes from an in-memory fixture
//! rather than a process-global environment variable.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration as StdDuration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, SaltString};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::macros::datetime;
use tower::ServiceExt;

use telegraph_center::blob::BlobStore;
use telegraph_center::config::{
    AppConfig, ClientConfig, DataConfig, OperatorConfig, ServerConfig, SonioxConfig,
};
use telegraph_center::http::{AppState, Clock, IdGenerator};
use telegraph_center::monitor::auth::{AuthService, StaticPasswordHash};
use telegraph_center::monitor::futures_sleep::BoxSleep;
use telegraph_center::monitor::{MonitorState, Sleeper};
use telegraph_center::storage::{NewLoginFailure, SqliteStore};

const NOW: OffsetDateTime = datetime!(2026-06-24 12:00:00 UTC);
const USERNAME: &str = "break";
const PASSWORD: &str = "correct-horse-battery";
const SESSION_COOKIE: &str = "telegraph_operator_session";

struct FixedClock(OffsetDateTime);
impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

struct SequentialIds(AtomicU64);
impl IdGenerator for SequentialIds {
    fn generate(&self) -> String {
        format!("id-{}", self.0.fetch_add(1, Ordering::SeqCst))
    }
}

/// A sleeper that records requested delays instead of waiting.
struct RecordingSleeper {
    delays: Arc<Mutex<Vec<StdDuration>>>,
}
impl Sleeper for RecordingSleeper {
    fn sleep(&self, duration: StdDuration) -> BoxSleep<'_> {
        self.delays.lock().unwrap().push(duration);
        Box::pin(async {})
    }
}

/// Mint an Argon2id PHC hash for the fixture password with a fixed salt.
fn fixture_hash() -> String {
    let salt = SaltString::encode_b64(b"telegraph-monitor-salt").unwrap();
    Argon2::default()
        .hash_password(PASSWORD.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

struct Harness {
    _dir: TempDir,
    store: SqliteStore,
    state: MonitorState,
    delays: Arc<Mutex<Vec<StdDuration>>>,
}

impl Harness {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = SqliteStore::connect(dir.path().join("telegraph.db"))
            .await
            .unwrap();
        let delays = Arc::new(Mutex::new(Vec::new()));
        let state = MonitorState {
            store: store.clone(),
            clock: Arc::new(FixedClock(NOW)),
            ids: Arc::new(SequentialIds(AtomicU64::new(1))),
            auth: Arc::new(AuthService::new(
                USERNAME,
                Arc::new(StaticPasswordHash(fixture_hash())),
            )),
            sleeper: Arc::new(RecordingSleeper {
                delays: delays.clone(),
            }),
        };
        Self {
            _dir: dir,
            store,
            state,
            delays,
        }
    }

    fn router(&self) -> Router {
        telegraph_center::monitor::router(self.state.clone())
    }

    async fn session_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM operator_sessions")
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }

    async fn failure_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM login_failures")
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }
}

struct Sent {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(router: Router, request: Request<Body>) -> Sent {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Sent {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"));
    }
    builder.body(Body::empty()).unwrap()
}

fn post_form(uri: &str, cookie: Option<&str>, body: String) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"));
    }
    builder.body(Body::from(body)).unwrap()
}

fn login_request(username: &str, password: &str) -> Request<Body> {
    post_form(
        "/monitor/login",
        None,
        format!("username={username}&password={password}"),
    )
}

fn set_cookie(headers: &HeaderMap) -> Option<String> {
    Some(headers.get(header::SET_COOKIE)?.to_str().ok()?.to_string())
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = set_cookie(headers)?;
    let first = cookie.split(';').next()?;
    let (name, value) = first.split_once('=')?;
    if name == SESSION_COOKIE && !value.is_empty() {
        Some(value.to_string())
    } else {
        None
    }
}

fn extract_csrf(html: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let start = html.find(marker).expect("csrf field present") + marker.len();
    let rest = &html[start..];
    let end = rest.find('"').expect("closing quote");
    rest[..end].to_string()
}

/// Log in and return the raw session token from the cookie.
async fn login(harness: &Harness) -> String {
    let sent = send(harness.router(), login_request(USERNAME, PASSWORD)).await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "login should succeed");
    session_token(&sent.headers).expect("session cookie set")
}

#[tokio::test]
async fn unauthenticated_monitor_redirects_to_login() {
    let harness = Harness::new().await;
    let sent = send(harness.router(), get("/monitor", None)).await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER);
    assert_eq!(
        sent.headers.get(header::LOCATION).unwrap(),
        "/monitor/login"
    );
}

#[tokio::test]
async fn login_page_renders_html_form() {
    let harness = Harness::new().await;
    let sent = send(harness.router(), get("/monitor/login", None)).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert!(
        sent.headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(sent.body.contains("<form"));
    assert!(sent.body.contains("Sign in"));
}

#[tokio::test]
async fn successful_login_sets_secure_cookie_and_creates_session() {
    let harness = Harness::new().await;
    let sent = send(harness.router(), login_request(USERNAME, PASSWORD)).await;

    assert_eq!(sent.status, StatusCode::SEE_OTHER);
    assert_eq!(sent.headers.get(header::LOCATION).unwrap(), "/monitor");

    let cookie = set_cookie(&sent.headers).expect("Set-Cookie present");
    assert!(cookie.starts_with(&format!("{SESSION_COOKIE}=")));
    assert!(cookie.contains("Path=/monitor"));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("Secure"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("Max-Age="));

    assert_eq!(harness.session_count().await, 1);
}

#[tokio::test]
async fn wrong_password_records_failure_without_a_session() {
    let harness = Harness::new().await;
    let sent = send(harness.router(), login_request(USERNAME, "nope")).await;

    assert_eq!(sent.status, StatusCode::UNAUTHORIZED);
    assert!(set_cookie(&sent.headers).is_none());
    assert!(sent.body.contains("Invalid username or password."));

    assert_eq!(harness.failure_count().await, 1);
    assert_eq!(harness.session_count().await, 0);
}

#[tokio::test]
async fn lockout_blocks_login_without_revealing_account_existence() {
    let harness = Harness::new().await;
    for i in 0..10 {
        harness
            .store
            .record_login_failure(NewLoginFailure {
                id: format!("seed-{i}"),
                username: USERNAME.to_string(),
                remote_ip: "unknown".to_string(),
                failed_at: NOW,
            })
            .await
            .unwrap();
    }

    // The real Operator, with the correct password, is locked out.
    let real = send(harness.router(), login_request(USERNAME, PASSWORD)).await;
    assert_eq!(real.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(set_cookie(&real.headers).is_none());
    assert_eq!(harness.session_count().await, 0);

    // An unknown account gets the identical generic response.
    let ghost = send(harness.router(), login_request("ghost", "whatever")).await;
    assert_eq!(ghost.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(real.body, ghost.body);
    assert!(!real.body.contains(USERNAME));
}

#[tokio::test]
async fn delay_threshold_throttles_before_a_successful_login() {
    let harness = Harness::new().await;
    for i in 0..5 {
        harness
            .store
            .record_login_failure(NewLoginFailure {
                id: format!("seed-{i}"),
                username: USERNAME.to_string(),
                remote_ip: "unknown".to_string(),
                failed_at: NOW,
            })
            .await
            .unwrap();
    }

    let sent = send(harness.router(), login_request(USERNAME, PASSWORD)).await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER);
    // The handler enforced a 2s throttle delay through the injected sleeper, and
    // a successful login cleared the failures.
    assert_eq!(
        *harness.delays.lock().unwrap(),
        vec![StdDuration::from_secs(2)]
    );
    assert_eq!(harness.failure_count().await, 0);
}

#[tokio::test]
async fn authenticated_monitor_page_renders() {
    let harness = Harness::new().await;
    let token = login(&harness).await;

    let sent = send(harness.router(), get("/monitor", Some(&token))).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert!(sent.body.contains("Signed in as break"));
    assert!(sent.body.contains("Log out"));
}

#[tokio::test]
async fn logout_requires_a_valid_csrf_token() {
    let harness = Harness::new().await;
    let token = login(&harness).await;

    // Wrong token is rejected without revoking the session.
    let bad = send(
        harness.router(),
        post_form("/monitor/logout", Some(&token), "csrf_token=wrong".into()),
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);

    // Missing token is also rejected.
    let missing = send(
        harness.router(),
        post_form("/monitor/logout", Some(&token), String::new()),
    )
    .await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);

    // The session is still usable after CSRF failures.
    let still_in = send(harness.router(), get("/monitor", Some(&token))).await;
    assert_eq!(still_in.status, StatusCode::OK);
}

#[tokio::test]
async fn valid_logout_revokes_session_and_clears_cookie() {
    let harness = Harness::new().await;
    let token = login(&harness).await;

    // Read the CSRF token rendered into the monitor page's logout form.
    let page = send(harness.router(), get("/monitor", Some(&token))).await;
    let csrf = extract_csrf(&page.body);

    let sent = send(
        harness.router(),
        post_form(
            "/monitor/logout",
            Some(&token),
            format!("csrf_token={csrf}"),
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER);
    assert_eq!(
        sent.headers.get(header::LOCATION).unwrap(),
        "/monitor/login"
    );

    let cookie = set_cookie(&sent.headers).expect("clearing cookie");
    assert!(cookie.contains("Path=/monitor"));
    assert!(cookie.contains("Max-Age=0"));

    // The revoked session no longer grants access.
    let after = send(harness.router(), get("/monitor", Some(&token))).await;
    assert_eq!(after.status, StatusCode::SEE_OTHER);
    assert_eq!(
        after.headers.get(header::LOCATION).unwrap(),
        "/monitor/login"
    );
}

#[tokio::test]
async fn api_remains_independent_of_operator_auth() {
    let dir = TempDir::new().unwrap();
    let store = SqliteStore::connect(dir.path().join("telegraph.db"))
        .await
        .unwrap();
    let blobs = BlobStore::new(dir.path()).await.unwrap();
    let config = AppConfig {
        server: ServerConfig {
            listen: "127.0.0.1:8080".into(),
            public_base_path: String::new(),
        },
        data: DataConfig {
            dir: dir.path().to_path_buf(),
            max_upload_bytes: 1_000_000,
        },
        clients: vec![ClientConfig {
            name: "litewatch-main".into(),
            certificate_fingerprint: "sha256:good".into(),
        }],
        operator: OperatorConfig {
            username: USERNAME.into(),
            password_hash_env: "TELEGRAPH_OPERATOR_PASSWORD_HASH".into(),
        },
        soniox: SonioxConfig {
            api_key_env: "SONIOX_API_KEY".into(),
            model: "stt-async-v5".into(),
            language_hints: vec!["en".into()],
            enable_speaker_diarization: true,
            enable_language_identification: false,
        },
        sinks: vec![],
    };

    let api = AppState {
        config: Arc::new(config),
        store: store.clone(),
        blobs: Arc::new(blobs),
        clock: Arc::new(FixedClock(NOW)),
        ids: Arc::new(SequentialIds(AtomicU64::new(1))),
    };
    let monitor = MonitorState {
        store,
        clock: Arc::new(FixedClock(NOW)),
        ids: Arc::new(SequentialIds(AtomicU64::new(1))),
        auth: Arc::new(AuthService::new(
            USERNAME,
            Arc::new(StaticPasswordHash(fixture_hash())),
        )),
        sleeper: Arc::new(RecordingSleeper {
            delays: Arc::new(Mutex::new(Vec::new())),
        }),
    };

    let app = telegraph_center::http::app(api, monitor);

    // The API rejects on its own terms (missing client identity), not with a
    // monitor login redirect, and never consults an Operator Session. A valid
    // multipart content-type lets the handler run and reach its own auth check.
    let request = Request::builder()
        .method("POST")
        .uri("/api/recordings")
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=boundary123",
        )
        .body(Body::empty())
        .unwrap();
    let sent = send(app, request).await;
    assert_eq!(sent.status, StatusCode::UNAUTHORIZED);
    assert!(sent.body.contains("missing_client_identity"));
}
