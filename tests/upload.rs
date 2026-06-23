//! Upload handler tests driven through Tower service calls with temp storage.
//!
//! No network is used: requests are constructed as raw multipart bodies and
//! sent through `oneshot`. Time and Recording ids are injected for determinism.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::macros::datetime;
use tower::ServiceExt;

use telegraph_center::blob::BlobStore;
use telegraph_center::config::{
    AppConfig, ClientConfig, DataConfig, OperatorConfig, ServerConfig, SonioxConfig,
};
use telegraph_center::http::{AppState, Clock, IdGenerator, router};
use telegraph_center::storage::SqliteStore;

const NOW: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);
const GOOD_FINGERPRINT: &str = "sha256:good";

struct FixedClock(OffsetDateTime);
impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
}

struct SequentialIds(AtomicU64);
impl IdGenerator for SequentialIds {
    fn generate(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        format!("rec-{n}")
    }
}

struct Harness {
    _dir: TempDir,
    state: AppState,
}

impl Harness {
    async fn with_limit(max_upload_bytes: u64) -> Self {
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
                max_upload_bytes,
            },
            clients: vec![ClientConfig {
                name: "litewatch-main".into(),
                certificate_fingerprint: GOOD_FINGERPRINT.into(),
            }],
            operator: OperatorConfig {
                username: "break".into(),
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

        let state = AppState {
            config: Arc::new(config),
            store,
            blobs: Arc::new(blobs),
            clock: Arc::new(FixedClock(NOW)),
            ids: Arc::new(SequentialIds(AtomicU64::new(1))),
        };
        Self { _dir: dir, state }
    }

    async fn default() -> Self {
        Self::with_limit(1_000_000).await
    }

    fn data_dir(&self) -> &std::path::Path {
        self.state.blobs.data_dir()
    }
}

/// A single multipart part.
struct Part {
    name: &'static str,
    filename: Option<&'static str>,
    content_type: Option<&'static str>,
    data: Vec<u8>,
}

impl Part {
    fn text(name: &'static str, value: &str) -> Self {
        Part {
            name,
            filename: None,
            content_type: None,
            data: value.as_bytes().to_vec(),
        }
    }

    fn file(name: &'static str, filename: &'static str, data: Vec<u8>) -> Self {
        Part {
            name,
            filename: Some(filename),
            content_type: Some("application/octet-stream"),
            data,
        }
    }
}

const BOUNDARY: &str = "telegraphtestboundary";

fn multipart_body(parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
        if let Some(filename) = part.filename {
            disposition.push_str(&format!("; filename=\"{filename}\""));
        }
        disposition.push_str("\r\n");
        body.extend_from_slice(disposition.as_bytes());
        if let Some(content_type) = part.content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Build a request with the standard good headers, overridable per test.
struct RequestSpec {
    fingerprint: Option<&'static str>,
    recording_id: Option<&'static str>,
    parts: Vec<Part>,
}

impl RequestSpec {
    fn new(parts: Vec<Part>) -> Self {
        RequestSpec {
            fingerprint: Some(GOOD_FINGERPRINT),
            recording_id: Some("cr-1"),
            parts,
        }
    }

    fn build(self) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/recordings")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            );
        if let Some(fp) = self.fingerprint {
            builder = builder.header("x-telegraph-client-fingerprint", fp);
        }
        if let Some(id) = self.recording_id {
            builder = builder.header("x-telegraph-client-recording-id", id);
        }
        builder
            .body(Body::from(multipart_body(&self.parts)))
            .unwrap()
    }
}

/// A minimal valid PCM WAV (16 kHz / 16-bit / mono) with `data_len` data bytes.
fn tiny_wav(data_len: u32) -> Vec<u8> {
    let sample_rate = 16_000u32;
    let channels = 1u16;
    let bits = 16u16;
    let byte_rate = sample_rate * u32::from(channels) * (u32::from(bits) / 8);
    let block_align = channels * (bits / 8);
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&byte_rate.to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, data_len as usize));
    v
}

async fn send(harness: &Harness, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router(harness.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

fn count_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map(|rd| rd.count()).unwrap_or(0)
}

#[tokio::test]
async fn successful_upload_creates_recording_and_blob() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![Part::file(
        "audio",
        "20260623-120000.wav",
        tiny_wav(3200),
    )])
    .build();

    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["recording_id"], "rec-1");
    assert_eq!(json["status"], "received");
    assert!(json.get("duplicate").is_none());

    // The final blob exists at a server-generated path.
    let blob = harness.data_dir().join("recordings").join("rec-1.wav");
    assert!(blob.exists());
    // The temp dir has no leftovers.
    assert_eq!(count_files(&harness.data_dir().join("tmp")), 0);

    // Stored metadata: original filename kept as metadata, server-side path,
    // and extracted WAV parameters.
    let recording = harness
        .state
        .store
        .get_recording("rec-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recording.original_filename.as_deref(),
        Some("20260623-120000.wav")
    );
    assert_eq!(recording.blob_path.as_deref(), Some("recordings/rec-1.wav"));
    assert_eq!(recording.sample_rate_hz, Some(16_000));
    assert_eq!(recording.channels, Some(1));
    assert_eq!(recording.bits_per_sample, Some(16));
    assert_eq!(recording.audio_duration_ms, Some(100));
    assert_eq!(recording.received_at, NOW);
}

#[tokio::test]
async fn upload_with_tags_and_recorded_at() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![
        Part::text("tags", r#"["Journal", "diary"]"#),
        Part::text("recorded_at", "2026-06-23T11:59:00Z"),
        Part::file("audio", "rec.wav", tiny_wav(320)),
    ])
    .build();

    let (status, _json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::CREATED);

    let recording = harness
        .state
        .store
        .get_recording("rec-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recording.tags.as_slice(),
        &["journal".to_string(), "diary".to_string()]
    );
    assert_eq!(
        recording.recorded_at,
        Some(datetime!(2026-06-23 11:59:00 UTC))
    );
}

#[tokio::test]
async fn duplicate_recording_id_returns_existing_without_second_row() {
    let harness = Harness::default().await;

    let first = RequestSpec::new(vec![Part::file("audio", "a.wav", tiny_wav(320))]).build();
    let (status, json) = send(&harness, first).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["recording_id"], "rec-1");

    // Same client + client_recording_id: duplicate.
    let second = RequestSpec::new(vec![Part::file("audio", "b.wav", tiny_wav(320))]).build();
    let (status, json) = send(&harness, second).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["recording_id"], "rec-1");
    assert_eq!(json["duplicate"], true);

    // Only one row, and only one blob, exist.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recordings")
        .fetch_one(harness.state.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(count_files(&harness.data_dir().join("recordings")), 1);
}

#[tokio::test]
async fn missing_fingerprint_is_unauthorized() {
    let harness = Harness::default().await;
    let mut spec = RequestSpec::new(vec![Part::file("audio", "a.wav", tiny_wav(320))]);
    spec.fingerprint = None;
    let (status, json) = send(&harness, spec.build()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"], "missing_client_identity");
}

#[tokio::test]
async fn unknown_fingerprint_is_forbidden() {
    let harness = Harness::default().await;
    let mut spec = RequestSpec::new(vec![Part::file("audio", "a.wav", tiny_wav(320))]);
    spec.fingerprint = Some("sha256:unknown");
    let (status, json) = send(&harness, spec.build()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"], "unknown_client");
}

#[tokio::test]
async fn missing_recording_id_is_bad_request() {
    let harness = Harness::default().await;
    let mut spec = RequestSpec::new(vec![Part::file("audio", "a.wav", tiny_wav(320))]);
    spec.recording_id = None;
    let (status, json) = send(&harness, spec.build()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "missing_recording_id");
}

#[tokio::test]
async fn blank_recording_id_is_bad_request() {
    let harness = Harness::default().await;
    let mut spec = RequestSpec::new(vec![Part::file("audio", "a.wav", tiny_wav(320))]);
    spec.recording_id = Some("   ");
    let (status, json) = send(&harness, spec.build()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "missing_recording_id");
}

#[tokio::test]
async fn missing_audio_is_bad_request() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![Part::text("tags", "[\"journal\"]")]).build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "missing_audio");
}

#[tokio::test]
async fn multiple_audio_fields_are_bad_request() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![
        Part::file("audio", "a.wav", tiny_wav(320)),
        Part::file("audio", "b.wav", tiny_wav(320)),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "multiple_audio");
}

#[tokio::test]
async fn invalid_tags_json_is_bad_request() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![
        Part::text("tags", "not json"),
        Part::file("audio", "a.wav", tiny_wav(320)),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "invalid_tags");
}

#[tokio::test]
async fn invalid_tag_value_is_bad_request() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![
        Part::text("tags", r#"["not a tag"]"#),
        Part::file("audio", "a.wav", tiny_wav(320)),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "invalid_tags");
}

#[tokio::test]
async fn malformed_recorded_at_is_bad_request() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![
        Part::text("recorded_at", "yesterday"),
        Part::file("audio", "a.wav", tiny_wav(320)),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "invalid_recorded_at");
}

#[tokio::test]
async fn future_recorded_at_is_bad_request() {
    let harness = Harness::default().await;
    // NOW is 2026-06-23 12:00; more than 24h ahead.
    let request = RequestSpec::new(vec![
        Part::text("recorded_at", "2026-06-25T12:00:01Z"),
        Part::file("audio", "a.wav", tiny_wav(320)),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "recorded_at_in_future");
}

#[tokio::test]
async fn non_wav_payload_is_unsupported_media_type() {
    let harness = Harness::default().await;
    let request = RequestSpec::new(vec![Part::file(
        "audio",
        "a.wav",
        b"this is not a wav".to_vec(),
    )])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(json["error"], "unsupported_audio");
    // No row or blob was created.
    assert_eq!(count_files(&harness.data_dir().join("recordings")), 0);
    assert_eq!(count_files(&harness.data_dir().join("tmp")), 0);
}

#[tokio::test]
async fn oversize_upload_is_rejected_and_leaves_no_files() {
    let harness = Harness::with_limit(128).await;
    let request = RequestSpec::new(vec![Part::file("audio", "a.wav", tiny_wav(4096))]).build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["error"], "payload_too_large");

    // No temp or final blob remains.
    assert_eq!(count_files(&harness.data_dir().join("tmp")), 0);
    assert_eq!(count_files(&harness.data_dir().join("recordings")), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recordings")
        .fetch_one(harness.state.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn oversize_tags_field_cannot_bypass_limit() {
    // Audio fits comfortably under the limit, but a large non-audio field must
    // not be allowed to exceed the overall upload budget.
    let harness = Harness::with_limit(2048).await;
    let big = "x".repeat(5000);
    let request = RequestSpec::new(vec![
        Part::file("audio", "a.wav", tiny_wav(320)),
        Part::text("tags", &big),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["error"], "payload_too_large");

    assert_eq!(count_files(&harness.data_dir().join("tmp")), 0);
    assert_eq!(count_files(&harness.data_dir().join("recordings")), 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recordings")
        .fetch_one(harness.state.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn oversize_unknown_field_cannot_bypass_limit() {
    // Ignored fields are still charged against the budget.
    let harness = Harness::with_limit(2048).await;
    let big = "x".repeat(5000);
    let request = RequestSpec::new(vec![
        Part::file("audio", "a.wav", tiny_wav(320)),
        Part::text("junk", &big),
    ])
    .build();
    let (status, json) = send(&harness, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["error"], "payload_too_large");

    assert_eq!(count_files(&harness.data_dir().join("tmp")), 0);
    assert_eq!(count_files(&harness.data_dir().join("recordings")), 0);
}
