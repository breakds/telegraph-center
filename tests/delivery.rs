//! Webhook Sink Delivery tests: payload/headers/signature via a fake transport,
//! plus routing→delivery worker integration. No Hermes, network, or secrets.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tempfile::TempDir;
use time::OffsetDateTime;
use time::macros::datetime;

use telegraph_center::blob::BlobStore;
use telegraph_center::config::{AppConfig, Sink, WebhookSink};
use telegraph_center::delivery::webhook::{WebhookHttpRequest, WebhookResponse, WebhookTransport};
use telegraph_center::delivery::{ResolvedSink, WebhookBuildError, WebhookSinkClient, hmac};
use telegraph_center::domain::{Delivery, DeliveryStatus, Recording, RecordingStatus, Tags};
use telegraph_center::routing::ConfigRouter;
use telegraph_center::seam::Clock;
use telegraph_center::storage::{NewRecording, NewTranscript, SqliteStore};
use telegraph_center::workers::{
    DeliveryRequest, SinkClient, WorkerContext, WorkerFailure, delivery, routing,
};

const WHEN: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);
const SECRET: &[u8] = b"hermes-secret";

fn at(seconds: i64) -> OffsetDateTime {
    WHEN + time::Duration::seconds(seconds)
}

// --- Seams -----------------------------------------------------------------

struct FixedClock(OffsetDateTime);
impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

struct SequentialIds(std::sync::atomic::AtomicU64);
impl telegraph_center::seam::IdGenerator for SequentialIds {
    fn generate(&self) -> String {
        format!(
            "id-{}",
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )
    }
}

// --- Fake webhook transport ------------------------------------------------

#[derive(Clone)]
struct CapturedRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct FakeWebhookTransport {
    response: Result<WebhookResponse, String>,
    captured: Arc<Mutex<Option<CapturedRequest>>>,
}

impl FakeWebhookTransport {
    fn new(
        response: Result<WebhookResponse, String>,
    ) -> (Self, Arc<Mutex<Option<CapturedRequest>>>) {
        let captured = Arc::new(Mutex::new(None));
        (
            Self {
                response,
                captured: captured.clone(),
            },
            captured,
        )
    }
}

impl WebhookTransport for FakeWebhookTransport {
    async fn post(&self, request: WebhookHttpRequest<'_>) -> Result<WebhookResponse, String> {
        *self.captured.lock().unwrap() = Some(CapturedRequest {
            url: request.url.to_string(),
            headers: request
                .headers
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
            body: request.body.clone(),
        });
        self.response.clone()
    }
}

fn ok(status: u16) -> Result<WebhookResponse, String> {
    Ok(WebhookResponse {
        status,
        body_excerpt: String::new(),
    })
}

// --- Domain builders -------------------------------------------------------

fn recording() -> Recording {
    Recording {
        id: "rec-1".to_string(),
        client_id: "litewatch-main".to_string(),
        client_recording_id: "cr-1".to_string(),
        status: RecordingStatus::Delivering,
        original_filename: None,
        blob_path: Some("recordings/rec-1.wav".to_string()),
        audio_size_bytes: Some(1000),
        audio_duration_ms: Some(1000),
        sample_rate_hz: Some(16000),
        channels: Some(1),
        bits_per_sample: Some(16),
        tags: Tags::new(["journal"]).unwrap(),
        recorded_at: None,
        received_at: WHEN,
        selected_sink_name: Some("journal".to_string()),
        latest_error: None,
        created_at: WHEN,
        updated_at: WHEN,
    }
}

fn delivery(sink_name: &str) -> Delivery {
    Delivery {
        id: "del-1".to_string(),
        recording_id: "rec-1".to_string(),
        sink_name: sink_name.to_string(),
        status: DeliveryStatus::Delivering,
        selected_at: WHEN,
        completed_at: None,
        retry_deadline_at: Some(at(86_400)),
        latest_error: None,
    }
}

fn transcript() -> telegraph_center::domain::Transcript {
    telegraph_center::domain::Transcript {
        recording_id: "rec-1".to_string(),
        provider: "soniox".to_string(),
        text: "hello world".to_string(),
        raw_json: "{}".to_string(),
        provider_file_id: None,
        provider_transcription_id: None,
        created_at: WHEN,
    }
}

fn journal_client(
    response: Result<WebhookResponse, String>,
) -> (
    WebhookSinkClient<FakeWebhookTransport>,
    Arc<Mutex<Option<CapturedRequest>>>,
) {
    let (transport, captured) = FakeWebhookTransport::new(response);
    let client = WebhookSinkClient::with_transport(
        vec![ResolvedSink::new(
            "journal",
            "http://hermes.local/webhooks/journal",
            SECRET.to_vec(),
        )],
        transport,
    );
    (client, captured)
}

async fn deliver(
    client: &WebhookSinkClient<FakeWebhookTransport>,
    sink_name: &str,
) -> Result<(), WorkerFailure> {
    let recording = recording();
    let delivery = delivery(sink_name);
    let transcript = transcript();
    client
        .deliver(DeliveryRequest {
            recording: &recording,
            delivery: &delivery,
            transcript: &transcript,
        })
        .await
}

// --- Webhook Sink client tests ---------------------------------------------

#[tokio::test]
async fn successful_delivery_sends_signed_request_to_configured_url() {
    let (client, captured) = journal_client(ok(200));
    deliver(&client, "journal").await.unwrap();

    let request = captured
        .lock()
        .unwrap()
        .clone()
        .expect("a request was sent");
    assert_eq!(request.url, "http://hermes.local/webhooks/journal");
    assert_eq!(request.header("Content-Type"), Some("application/json"));
    assert_eq!(request.header("X-Request-ID"), Some("del-1"));
    assert_eq!(request.header("X-Telegraph-Delivery-Id"), Some("del-1"));

    // The signature is HMAC-SHA256 over the exact bytes that were sent.
    let expected = hmac::sign(SECRET, &request.body);
    assert_eq!(
        request.header("X-Webhook-Signature"),
        Some(expected.as_str())
    );

    // The body is the agreed payload, with no audio or provider internals.
    let payload: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(payload["event"], "recording.transcribed");
    assert_eq!(payload["delivery_id"], "del-1");
    assert_eq!(payload["transcript"]["text"], "hello world");
    assert!(!String::from_utf8_lossy(&request.body).contains("raw_json"));
}

#[tokio::test]
async fn network_error_is_retryable() {
    let (client, _) = journal_client(Err("connection refused".to_string()));
    let failure = deliver(&client, "journal").await.unwrap_err();
    assert!(matches!(failure, WorkerFailure::Retryable { .. }));
}

#[tokio::test]
async fn server_error_and_rate_limit_are_retryable() {
    for status in [500, 503, 429] {
        let (client, _) = journal_client(ok(status));
        let failure = deliver(&client, "journal").await.unwrap_err();
        assert!(
            matches!(failure, WorkerFailure::Retryable { .. }),
            "status {status} should be retryable"
        );
    }
}

#[tokio::test]
async fn non_429_client_error_is_terminal() {
    for status in [400, 401, 403, 404, 409, 422] {
        let (client, _) = journal_client(ok(status));
        let failure = deliver(&client, "journal").await.unwrap_err();
        assert!(
            matches!(failure, WorkerFailure::Terminal { .. }),
            "status {status} should be terminal"
        );
    }
}

#[tokio::test]
async fn unknown_sink_is_terminal() {
    let (client, _) = journal_client(ok(200));
    let failure = deliver(&client, "nonexistent").await.unwrap_err();
    let WorkerFailure::Terminal { code, .. } = failure else {
        panic!("expected terminal failure");
    };
    assert_eq!(code, "unknown_sink");
}

#[test]
fn missing_secret_is_rejected_at_construction() {
    unsafe { std::env::remove_var("M5_MISSING_SECRET") };
    let sinks = vec![Sink::Webhook(WebhookSink {
        name: "journal".to_string(),
        url: "http://hermes.local/webhooks/journal".to_string(),
        secret_env: "M5_MISSING_SECRET".to_string(),
        match_tags: Tags::default(),
    })];
    let result = WebhookSinkClient::from_config(&sinks, reqwest::Client::new());
    assert!(matches!(result, Err(WebhookBuildError::MissingSecret(_))));
}

#[test]
fn blank_secret_is_rejected_at_construction() {
    unsafe { std::env::set_var("M5_BLANK_SECRET", "   ") };
    let sinks = vec![Sink::Webhook(WebhookSink {
        name: "journal".to_string(),
        url: "http://hermes.local/webhooks/journal".to_string(),
        secret_env: "M5_BLANK_SECRET".to_string(),
        match_tags: Tags::default(),
    })];
    let result = WebhookSinkClient::from_config(&sinks, reqwest::Client::new());
    assert!(matches!(result, Err(WebhookBuildError::BlankSecret(_))));
    unsafe { std::env::remove_var("M5_BLANK_SECRET") };
}

// --- Routing + Delivery worker integration ---------------------------------

async fn harness() -> (TempDir, SqliteStore, BlobStore, SequentialIds) {
    let dir = TempDir::new().unwrap();
    let store = SqliteStore::connect(dir.path().join("telegraph.db"))
        .await
        .unwrap();
    let blobs = BlobStore::new(dir.path()).await.unwrap();
    (
        dir,
        store,
        blobs,
        SequentialIds(std::sync::atomic::AtomicU64::new(1)),
    )
}

async fn seed_routing(store: &SqliteStore, id: &str, tags: &[&str]) {
    store
        .create_recording(NewRecording {
            id: id.to_string(),
            client_id: "litewatch-main".to_string(),
            client_recording_id: id.to_string(),
            original_filename: None,
            blob_path: Some(BlobStore::relative_path(id)),
            audio_size_bytes: Some(1000),
            audio_duration_ms: Some(1000),
            sample_rate_hz: Some(16000),
            channels: Some(1),
            bits_per_sample: Some(16),
            tags: Tags::new(tags.iter().copied()).unwrap(),
            recorded_at: None,
            received_at: at(0),
        })
        .await
        .unwrap();
    store
        .update_recording_status(id, RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .store_transcript(NewTranscript {
            recording_id: id.to_string(),
            provider: "soniox".to_string(),
            text: "hello world".to_string(),
            raw_json: "{}".to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: at(2),
        })
        .await
        .unwrap();
}

fn config_router() -> ConfigRouter {
    let toml = r#"
[data]
dir = "/var/lib/telegraph-center"

[operator]
username = "break"
password_hash_env = "TELEGRAPH_OPERATOR_PASSWORD_HASH"

[soniox]
api_key_env = "SONIOX_API_KEY"

[[sinks]]
name = "journal"
type = "webhook"
url = "http://hermes.local/webhooks/journal"
secret_env = "JOURNAL_SECRET"
match_tags = ["journal"]
"#;
    ConfigRouter::from_config(&AppConfig::from_toml_str(toml).unwrap())
}

#[tokio::test]
async fn routes_and_delivers_end_to_end() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_routing(&store, "rec-1", &["journal"]).await;

    routing::tick_once(&ctx, &config_router(), &FixedClock(at(0)))
        .await
        .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivering
    );

    let (client, _) = journal_client(ok(200));
    delivery::tick_once(&ctx, &client, &FixedClock(at(10)))
        .await
        .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivered
    );
}

#[tokio::test]
async fn no_matching_sink_is_backlogged() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_routing(&store, "rec-1", &["unrelated"]).await;

    routing::tick_once(&ctx, &config_router(), &FixedClock(at(0)))
        .await
        .unwrap();
    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Backlogged);
    assert!(recording.selected_sink_name.is_none());
}

#[tokio::test]
async fn webhook_503_keeps_delivering_before_deadline() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_routing(&store, "rec-1", &["journal"]).await;
    routing::tick_once(&ctx, &config_router(), &FixedClock(at(0)))
        .await
        .unwrap();

    let (client, _) = journal_client(ok(503));
    delivery::tick_once(&ctx, &client, &FixedClock(at(10)))
        .await
        .unwrap();

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Delivering);
    let delivery = store
        .get_delivery_for_recording("rec-1")
        .await
        .unwrap()
        .unwrap();
    let attempts = store.list_delivery_attempts(&delivery.id).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].retryable);
}

#[tokio::test]
async fn webhook_400_moves_to_delivery_failed_not_backlog() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_routing(&store, "rec-1", &["journal"]).await;
    routing::tick_once(&ctx, &config_router(), &FixedClock(at(0)))
        .await
        .unwrap();

    let (client, _) = journal_client(ok(400));
    delivery::tick_once(&ctx, &client, &FixedClock(at(10)))
        .await
        .unwrap();

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::DeliveryFailed);
    assert_ne!(recording.status, RecordingStatus::Backlogged);
}
