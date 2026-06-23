//! HTTP layer for the Client API.
//!
//! Handlers orchestrate; WAV parsing lives in [`crate::wav`] and blob storage
//! in [`crate::blob`]. The app is built with [`router`] so tests can drive it
//! through Tower service calls without binding a socket.

pub mod api;
pub mod error;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::post;

use crate::blob::BlobStore;
use crate::config::AppConfig;
use crate::storage::SqliteStore;

pub use crate::seam::{Clock, IdGenerator, SystemClock, UlidGenerator};

/// Shared state for the Client API. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    /// Validated application configuration.
    pub config: Arc<AppConfig>,
    /// SQLite-backed repository.
    pub store: SqliteStore,
    /// Filesystem blob storage.
    pub blobs: Arc<BlobStore>,
    /// Clock seam.
    pub clock: Arc<dyn Clock>,
    /// Recording-id generator seam.
    pub ids: Arc<dyn IdGenerator>,
}

/// Build the Client API router.
///
/// The default body limit is disabled because the upload handler enforces
/// [`crate::config::DataConfig::max_upload_bytes`] itself while streaming.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/recordings", post(api::upload_recording))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}
