//! Persistence layer for Telegraph Center state.
//!
//! SQLite is the source of truth for Recording state, attempts, routing, and
//! Operator Sessions. Repositories perform no filesystem writes, no HTTP calls,
//! and never read secrets from the environment; callers supply identifiers and
//! timestamps so behavior stays deterministic and testable.

pub mod sqlite;

use time::OffsetDateTime;

use crate::domain::{InvalidTransition, Tags};

pub use sqlite::SqliteStore;

/// A reason a storage operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// An error from the underlying database driver.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A migration failed to apply.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A requested Recording status transition is not permitted.
    #[error(transparent)]
    InvalidTransition(#[from] InvalidTransition),
    /// A Recording referenced by id does not exist.
    #[error("recording not found: {0:?}")]
    RecordingNotFound(String),
    /// A Delivery referenced by id does not exist.
    #[error("delivery not found: {0:?}")]
    DeliveryNotFound(String),
    /// Data read back from storage could not be interpreted.
    #[error("corrupt stored data: {0}")]
    Corrupt(String),
}

/// The outcome of a create-or-get Recording call.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingCreation {
    /// The created or pre-existing Recording.
    pub recording: crate::domain::Recording,
    /// Whether a new row was inserted (`false` means an idempotent duplicate).
    pub created: bool,
}

/// Input for creating a Recording.
#[derive(Debug, Clone)]
pub struct NewRecording {
    /// Server-generated stable identifier.
    pub id: String,
    /// Configured Client that submitted the Recording.
    pub client_id: String,
    /// Client-assigned identifier, unique within the Client.
    pub client_recording_id: String,
    /// Original upload filename, kept as metadata only.
    pub original_filename: Option<String>,
    /// Path to the finalized audio blob (populated in M2).
    pub blob_path: Option<String>,
    /// Stored audio size in bytes.
    pub audio_size_bytes: Option<i64>,
    /// Audio duration in milliseconds.
    pub audio_duration_ms: Option<i64>,
    /// Audio sample rate in hertz.
    pub sample_rate_hz: Option<i64>,
    /// Audio channel count.
    pub channels: Option<i64>,
    /// Audio bits per sample.
    pub bits_per_sample: Option<i64>,
    /// Normalized tags.
    pub tags: Tags,
    /// Optional Client-provided capture time.
    pub recorded_at: Option<OffsetDateTime>,
    /// Server receive time, also used as the row creation time.
    pub received_at: OffsetDateTime,
}

/// Input for storing a Transcript.
#[derive(Debug, Clone)]
pub struct NewTranscript {
    /// The Recording this Transcript belongs to.
    pub recording_id: String,
    /// Speech-to-text provider name.
    pub provider: String,
    /// Rendered plain-text Transcript.
    pub text: String,
    /// Raw provider JSON.
    pub raw_json: String,
    /// Provider-side file identifier, if any.
    pub provider_file_id: Option<String>,
    /// Provider-side transcription identifier, if any.
    pub provider_transcription_id: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// Input for recording a Transcription Attempt.
#[derive(Debug, Clone)]
pub struct NewTranscriptionAttempt {
    /// Stable attempt identifier.
    pub id: String,
    /// The Recording being transcribed.
    pub recording_id: String,
    /// 1-based attempt number.
    pub attempt_number: i64,
    /// When the attempt started.
    pub started_at: OffsetDateTime,
    /// When the attempt finished, if it did.
    pub finished_at: Option<OffsetDateTime>,
    /// Provider/worker-defined attempt status.
    pub status: String,
    /// Whether the failure (if any) is retryable.
    pub retryable: bool,
    /// Machine-readable error code, if any.
    pub error_code: Option<String>,
    /// Human-readable error message, if any.
    pub error_message: Option<String>,
}

/// Input for selecting a Sink and creating its Delivery.
#[derive(Debug, Clone)]
pub struct NewDelivery {
    /// Stable Delivery identifier, reused across retries.
    pub id: String,
    /// The Recording being delivered.
    pub recording_id: String,
    /// Name of the selected Sink.
    pub sink_name: String,
    /// When the Sink was selected.
    pub selected_at: OffsetDateTime,
    /// Deadline after which retries stop.
    pub retry_deadline_at: Option<OffsetDateTime>,
}

/// Input for recording a Delivery Attempt.
#[derive(Debug, Clone)]
pub struct NewDeliveryAttempt {
    /// Stable attempt identifier.
    pub id: String,
    /// The Delivery this attempt belongs to.
    pub delivery_id: String,
    /// 1-based attempt number.
    pub attempt_number: i64,
    /// When the attempt started.
    pub started_at: OffsetDateTime,
    /// When the attempt finished, if it did.
    pub finished_at: Option<OffsetDateTime>,
    /// Worker-defined attempt status.
    pub status: String,
    /// HTTP status code received, if any.
    pub http_status: Option<i64>,
    /// Whether the failure (if any) is retryable.
    pub retryable: bool,
    /// Human-readable error message, if any.
    pub error_message: Option<String>,
}

/// Input for creating an Operator Session.
#[derive(Debug, Clone)]
pub struct NewOperatorSession {
    /// Hash of the session token.
    pub session_hash: String,
    /// The Operator the session belongs to.
    pub operator_username: String,
    /// Hash of the per-session CSRF token.
    pub csrf_token_hash: String,
    /// Session creation time.
    pub created_at: OffsetDateTime,
    /// Last activity time.
    pub last_seen_at: OffsetDateTime,
    /// Idle expiry deadline.
    pub idle_expires_at: OffsetDateTime,
    /// Absolute expiry deadline.
    pub absolute_expires_at: OffsetDateTime,
}

/// Input for recording a failed login attempt.
#[derive(Debug, Clone)]
pub struct NewLoginFailure {
    /// Stable identifier.
    pub id: String,
    /// Attempted username.
    pub username: String,
    /// Remote IP of the attempt.
    pub remote_ip: String,
    /// When the failure occurred.
    pub failed_at: OffsetDateTime,
}

/// Input for appending an audit event.
#[derive(Debug, Clone)]
pub struct NewAuditEvent {
    /// Stable event identifier.
    pub id: String,
    /// When the event occurred.
    pub occurred_at: OffsetDateTime,
    /// Kind of actor (for example, `operator` or `system`).
    pub actor_kind: String,
    /// Identifier of the actor, if known.
    pub actor_id: Option<String>,
    /// Event type label.
    pub event_type: String,
    /// Related Recording, if any.
    pub recording_id: Option<String>,
    /// Structured details as JSON text.
    pub details_json: String,
}
