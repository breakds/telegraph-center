//! Runtime wiring tests: the binary's `build_states` + `http::app` produce a
//! router where both surfaces are mounted and independent — the monitor login
//! page is reachable and the Client API rejects a missing Client fingerprint.
//!
//! No network, no real secrets: the Operator password hash is read lazily on
//! login (not here), and these requests never reach the integrations.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tempfile::TempDir;
use tower::ServiceExt;

use telegraph_center::blob::BlobStore;
use telegraph_center::config::AppConfig;
use telegraph_center::http::{self, Clock, IdGenerator, SystemClock, UlidGenerator};
use telegraph_center::runtime::build_states;
use telegraph_center::storage::SqliteStore;

fn config_toml(data_dir: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:0"

[data]
dir = "{data_dir}"

[[clients]]
name = "litewatch-main"
certificate_fingerprint = "sha1:abc123"

[operator]
username = "break"
password_hash_env = "TELEGRAPH_OPERATOR_PASSWORD_HASH"

[soniox]
api_key_env = "SONIOX_API_KEY"

[[sinks]]
name = "journal"
type = "webhook"
url = "http://127.0.0.1:8644/webhooks/journal"
secret_env = "HERMES_JOURNAL_WEBHOOK_SECRET"
match_tags = ["journal"]
"#
    )
}

async fn build_app(dir: &TempDir) -> axum::Router {
    let data_dir = dir.path().to_str().unwrap();
    let config = Arc::new(AppConfig::from_toml_str(&config_toml(data_dir)).unwrap());
    let store = SqliteStore::connect(dir.path().join("telegraph.db"))
        .await
        .unwrap();
    let blobs = Arc::new(BlobStore::new(dir.path().to_path_buf()).await.unwrap());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids: Arc<dyn IdGenerator> = Arc::new(UlidGenerator);

    let (api, monitor) = build_states(config, store, blobs, clock, ids);
    http::app(api, monitor)
}

#[tokio::test]
async fn monitor_login_page_is_reachable() {
    let dir = TempDir::new().unwrap();
    let app = build_app(&dir).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/monitor/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_rejects_missing_client_fingerprint() {
    let dir = TempDir::new().unwrap();
    let app = build_app(&dir).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/recordings")
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=boundary",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"], "missing_client_identity");
}
