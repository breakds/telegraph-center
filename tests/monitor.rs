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
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

use telegraph_center::blob::BlobStore;
use telegraph_center::config::{
    AppConfig, ClientConfig, DataConfig, OperatorConfig, ServerConfig, Sink, SonioxConfig,
    WebhookSink,
};
use telegraph_center::domain::{RecordingStatus, Tags};
use telegraph_center::http::{AppState, Clock, IdGenerator};
use telegraph_center::monitor::auth::{AuthService, StaticPasswordHash};
use telegraph_center::monitor::futures_sleep::BoxSleep;
use telegraph_center::monitor::{MonitorState, Sleeper};
use telegraph_center::storage::{
    NewDelivery, NewDeliveryAttempt, NewLoginFailure, NewRecording, NewTranscript, SqliteStore,
};

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

/// A config with one configured Sink, for Manual Routing tests.
fn test_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            listen: "127.0.0.1:8080".into(),
            public_base_path: String::new(),
        },
        data: DataConfig {
            dir: "/tmp/telegraph-test".into(),
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
        sinks: vec![Sink::Webhook(WebhookSink {
            name: "journal".into(),
            url: "http://127.0.0.1:8644/webhooks/journal".into(),
            secret_env: "HERMES_JOURNAL_WEBHOOK_SECRET".into(),
            match_tags: Tags::new(["journal"]).unwrap(),
        })],
    }
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
            config: Arc::new(test_config()),
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

    // An unknown account gets a byte-for-byte identical generic response, so the
    // lockout reveals nothing about whether the username exists.
    let ghost = send(harness.router(), login_request("ghost", "whatever")).await;
    assert_eq!(ghost.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(real.body, ghost.body);
    assert!(real.body.contains("Too many attempts"));
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
    assert!(sent.body.contains("Recordings"));
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
    let config = Arc::new(test_config());

    let api = AppState {
        config: config.clone(),
        store: store.clone(),
        blobs: Arc::new(blobs),
        clock: Arc::new(FixedClock(NOW)),
        ids: Arc::new(SequentialIds(AtomicU64::new(1))),
    };
    let monitor = MonitorState {
        config,
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

// ---------------------------------------------------------------------------
// M7 monitor UI: list, detail, filters, manual routing, manual retry
// ---------------------------------------------------------------------------

fn seed_recording(id: &str, received_offset: i64) -> NewRecording {
    NewRecording {
        id: id.to_string(),
        client_id: "litewatch-main".to_string(),
        client_recording_id: format!("cr-{id}"),
        original_filename: Some("rec.wav".to_string()),
        blob_path: Some(format!("recordings/{id}.wav")),
        audio_size_bytes: Some(1_024),
        audio_duration_ms: Some(5_000),
        sample_rate_hz: Some(16_000),
        channels: Some(1),
        bits_per_sample: Some(16),
        tags: Tags::new(["journal"]).unwrap(),
        recorded_at: None,
        received_at: NOW + Duration::seconds(received_offset),
    }
}

async fn store_transcript(store: &SqliteStore, id: &str) {
    store
        .update_recording_status(id, RecordingStatus::Transcribing, NOW)
        .await
        .unwrap();
    store
        .store_transcript(NewTranscript {
            recording_id: id.to_string(),
            provider: "soniox".to_string(),
            text: "hello transcript world".to_string(),
            raw_json: r#"{"tokens":["hello"]}"#.to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: NOW,
        })
        .await
        .unwrap();
}

async fn seed_backlogged(store: &SqliteStore, id: &str) {
    store.create_recording(seed_recording(id, 0)).await.unwrap();
    store_transcript(store, id).await;
    store.mark_backlogged(id, NOW).await.unwrap();
}

async fn seed_delivering(store: &SqliteStore, id: &str) -> String {
    store.create_recording(seed_recording(id, 0)).await.unwrap();
    store_transcript(store, id).await;
    let delivery_id = format!("{id}-delivery");
    store
        .select_sink(NewDelivery {
            id: delivery_id.clone(),
            recording_id: id.to_string(),
            sink_name: "journal".to_string(),
            selected_at: NOW,
            retry_deadline_at: Some(NOW + Duration::hours(24)),
        })
        .await
        .unwrap();
    delivery_id
}

async fn seed_transcription_failed(store: &SqliteStore, id: &str) {
    store.create_recording(seed_recording(id, 0)).await.unwrap();
    store
        .update_recording_status(id, RecordingStatus::Transcribing, NOW)
        .await
        .unwrap();
    store
        .update_recording_status(id, RecordingStatus::TranscriptionFailed, NOW)
        .await
        .unwrap();
}

async fn seed_delivery_failed(store: &SqliteStore, id: &str) -> String {
    let delivery_id = seed_delivering(store, id).await;
    store
        .mark_delivery_failed(&delivery_id, NOW, "upstream said no")
        .await
        .unwrap();
    delivery_id
}

/// Log in and return the session token plus the page's CSRF token.
async fn login_with_csrf(harness: &Harness) -> (String, String) {
    let token = login(harness).await;
    let page = send(harness.router(), get("/monitor", Some(&token))).await;
    (token.clone(), extract_csrf(&page.body))
}

async fn recording_status(harness: &Harness, id: &str) -> RecordingStatus {
    harness
        .store
        .get_recording(id)
        .await
        .unwrap()
        .unwrap()
        .status
}

#[tokio::test]
async fn unauthenticated_detail_and_actions_redirect_to_login() {
    let harness = Harness::new().await;

    let detail = send(harness.router(), get("/monitor/recordings/x", None)).await;
    assert_eq!(detail.status, StatusCode::SEE_OTHER);
    assert_eq!(
        detail.headers.get(header::LOCATION).unwrap(),
        "/monitor/login"
    );

    let action = send(
        harness.router(),
        post_form(
            "/monitor/recordings/x/manual-route",
            None,
            "csrf_token=whatever&sink=journal".into(),
        ),
    )
    .await;
    assert_eq!(action.status, StatusCode::SEE_OTHER);
    assert_eq!(
        action.headers.get(header::LOCATION).unwrap(),
        "/monitor/login"
    );
}

#[tokio::test]
async fn list_shows_recordings_newest_first() {
    let harness = Harness::new().await;
    for (i, offset) in [0_i64, 100, 200].into_iter().enumerate() {
        harness
            .store
            .create_recording(seed_recording(&format!("r{i}"), offset))
            .await
            .unwrap();
    }
    let token = login(&harness).await;
    let page = send(harness.router(), get("/monitor", Some(&token))).await;
    assert_eq!(page.status, StatusCode::OK);

    // Newest (r2, offset 200) appears before r1 before r0.
    let p2 = page.body.find("cr-r2").unwrap();
    let p1 = page.body.find("cr-r1").unwrap();
    let p0 = page.body.find("cr-r0").unwrap();
    assert!(p2 < p1 && p1 < p0, "rows should be newest-first");
}

#[tokio::test]
async fn list_filters_select_expected_statuses() {
    let harness = Harness::new().await;
    seed_backlogged(&harness.store, "r-back").await;
    seed_transcription_failed(&harness.store, "r-tf").await;
    seed_delivering(&harness.store, "r-del").await;
    let token = login(&harness).await;

    let backlogged = send(
        harness.router(),
        get("/monitor?status=backlogged", Some(&token)),
    )
    .await;
    assert!(backlogged.body.contains("cr-r-back"));
    assert!(!backlogged.body.contains("cr-r-tf"));
    assert!(!backlogged.body.contains("cr-r-del"));

    let failed = send(
        harness.router(),
        get("/monitor?status=failed", Some(&token)),
    )
    .await;
    assert!(failed.body.contains("cr-r-tf"));
    assert!(!failed.body.contains("cr-r-back"));

    let delivering = send(
        harness.router(),
        get("/monitor?status=delivering", Some(&token)),
    )
    .await;
    assert!(delivering.body.contains("cr-r-del"));
    assert!(!delivering.body.contains("cr-r-back"));
}

#[tokio::test]
async fn detail_shows_metadata_transcript_sink_and_raw_json_without_audio_link() {
    let harness = Harness::new().await;
    let delivery_id = seed_delivering(&harness.store, "rec-1").await;
    harness
        .store
        .insert_delivery_attempt(NewDeliveryAttempt {
            id: "da-1".to_string(),
            delivery_id,
            attempt_number: 1,
            started_at: NOW,
            finished_at: Some(NOW + Duration::seconds(1)),
            status: "failed".to_string(),
            http_status: Some(503),
            retryable: true,
            error_message: Some("upstream".to_string()),
        })
        .await
        .unwrap();
    let token = login(&harness).await;

    let page = send(
        harness.router(),
        get("/monitor/recordings/rec-1", Some(&token)),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("hello transcript world"));
    assert!(page.body.contains("journal"));
    assert!(page.body.contains("<details>"));
    assert!(page.body.contains("tokens")); // raw provider JSON rendered
    assert!(page.body.contains("Delivery attempts"));

    // No audio playback or download is ever exposed.
    assert!(!page.body.contains("<audio"));
    assert!(!page.body.contains("recordings/rec-1.wav"));
    assert!(!page.body.contains("download"));
}

#[tokio::test]
async fn unknown_recording_detail_is_not_found() {
    let harness = Harness::new().await;
    let token = login(&harness).await;
    let page = send(
        harness.router(),
        get("/monitor/recordings/nope", Some(&token)),
    )
    .await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manual_routing_form_shows_only_for_backlogged() {
    let harness = Harness::new().await;
    seed_backlogged(&harness.store, "r-back").await;
    seed_delivering(&harness.store, "r-del").await;
    let token = login(&harness).await;

    let backlogged = send(
        harness.router(),
        get("/monitor/recordings/r-back", Some(&token)),
    )
    .await;
    assert!(
        backlogged
            .body
            .contains("/monitor/recordings/r-back/manual-route")
    );
    assert!(backlogged.body.contains("value=\"journal\""));

    let delivering = send(
        harness.router(),
        get("/monitor/recordings/r-del", Some(&token)),
    )
    .await;
    assert!(!delivering.body.contains("manual-route"));
}

#[tokio::test]
async fn manual_route_requires_csrf_and_creates_delivery() {
    let harness = Harness::new().await;
    seed_backlogged(&harness.store, "r-back").await;
    let (token, csrf) = login_with_csrf(&harness).await;

    // Bad CSRF changes nothing.
    let bad = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-back/manual-route",
            Some(&token),
            "csrf_token=wrong&sink=journal".into(),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    assert_eq!(
        recording_status(&harness, "r-back").await,
        RecordingStatus::Backlogged
    );

    // Unknown sink is rejected.
    let unknown = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-back/manual-route",
            Some(&token),
            format!("csrf_token={csrf}&sink=ghost"),
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert!(
        harness
            .store
            .get_delivery_for_recording("r-back")
            .await
            .unwrap()
            .is_none()
    );

    // Valid routing moves to delivering and creates the Delivery.
    let ok = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-back/manual-route",
            Some(&token),
            format!("csrf_token={csrf}&sink=journal"),
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(
        ok.headers.get(header::LOCATION).unwrap(),
        "/monitor/recordings/r-back"
    );
    assert_eq!(
        recording_status(&harness, "r-back").await,
        RecordingStatus::Delivering
    );
    let delivery = harness
        .store
        .get_delivery_for_recording("r-back")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.sink_name, "journal");
}

#[tokio::test]
async fn manual_retry_transcription_requires_csrf_and_transitions() {
    let harness = Harness::new().await;
    seed_transcription_failed(&harness.store, "r-tf").await;
    let (token, csrf) = login_with_csrf(&harness).await;

    let bad = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-tf/retry-transcription",
            Some(&token),
            "csrf_token=wrong".into(),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    assert_eq!(
        recording_status(&harness, "r-tf").await,
        RecordingStatus::TranscriptionFailed
    );

    let ok = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-tf/retry-transcription",
            Some(&token),
            format!("csrf_token={csrf}"),
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(
        recording_status(&harness, "r-tf").await,
        RecordingStatus::Transcribing
    );
}

#[tokio::test]
async fn manual_retry_delivery_requires_csrf_and_transitions() {
    let harness = Harness::new().await;
    seed_delivery_failed(&harness.store, "r-df").await;
    let (token, csrf) = login_with_csrf(&harness).await;

    let bad = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-df/retry-delivery",
            Some(&token),
            "csrf_token=wrong".into(),
        ),
    )
    .await;
    assert_eq!(bad.status, StatusCode::FORBIDDEN);
    assert_eq!(
        recording_status(&harness, "r-df").await,
        RecordingStatus::DeliveryFailed
    );

    let ok = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-df/retry-delivery",
            Some(&token),
            format!("csrf_token={csrf}"),
        ),
    )
    .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(
        recording_status(&harness, "r-df").await,
        RecordingStatus::Delivering
    );
}

#[tokio::test]
async fn retry_on_wrong_status_is_conflict() {
    let harness = Harness::new().await;
    seed_delivering(&harness.store, "r-del").await;
    let (token, csrf) = login_with_csrf(&harness).await;

    // Recording is delivering, not transcription_failed.
    let sent = send(
        harness.router(),
        post_form(
            "/monitor/recordings/r-del/retry-transcription",
            Some(&token),
            format!("csrf_token={csrf}"),
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CONFLICT);
    assert_eq!(
        recording_status(&harness, "r-del").await,
        RecordingStatus::Delivering
    );
}
