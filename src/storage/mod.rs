//! Persistence layer for Telegraph Center state.
//!
//! SQLite is the source of truth for Recording state, attempts, routing, and
//! Operator Sessions. Repositories perform no filesystem writes, no HTTP calls,
//! and never read secrets from the environment; callers supply identifiers and
//! timestamps so behavior stays deterministic and testable.

pub mod sqlite;

use time::OffsetDateTime;

use crate::domain::{
    AuditEvent, Delivery, DeliveryAttempt, InvalidTransition, Recording, RecordingStatus, Tags,
    Transcript, TranscriptionAttempt,
};

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
    /// Manual Routing was attempted on a Recording that is not Backlogged.
    #[error("recording {id:?} is not backlogged (status {status})")]
    RecordingNotBacklogged {
        /// The Recording id.
        id: String,
        /// The Recording's current status.
        status: String,
    },
    /// Manual Retry was attempted on a Recording not in the matching failed state.
    #[error("recording {id:?} is not in a retryable {kind} state (status {status})")]
    RecordingNotRetryable {
        /// The Recording id.
        id: String,
        /// The Recording's current status.
        status: String,
        /// Which kind of retry was attempted (`transcription` or `delivery`).
        kind: String,
    },
    /// Data read back from storage could not be interpreted.
    #[error("corrupt stored data: {0}")]
    Corrupt(String),
}

/// The outcome of an automatic routing attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum RoutingOutcome {
    /// A Sink was selected and the Delivery created.
    Selected(Delivery),
    /// Another worker already moved the Recording out of `routing`; nothing to do.
    AlreadyHandled,
}

/// Input for an Operator Manual Routing action on a Backlogged Recording.
#[derive(Debug, Clone)]
pub struct ManualRoute {
    /// The Backlogged Recording to route.
    pub recording_id: String,
    /// The configured Sink name selected by the Operator.
    pub sink_name: String,
    /// Stable Delivery identifier.
    pub delivery_id: String,
    /// Stable id for the appended audit event.
    pub audit_event_id: String,
    /// When the Sink was selected.
    pub selected_at: OffsetDateTime,
    /// Deadline after which Delivery retries stop.
    pub retry_deadline_at: OffsetDateTime,
    /// Operator identifier for the audit event, if known.
    pub actor_id: Option<String>,
}

/// Input for an Operator Manual Retry of failed Transcription.
#[derive(Debug, Clone)]
pub struct ManualRetryTranscription {
    /// The `transcription_failed` Recording to retry.
    pub recording_id: String,
    /// Stable id for the appended audit event.
    pub audit_event_id: String,
    /// When the retry was requested.
    pub at: OffsetDateTime,
    /// Operator identifier for the audit event, if known.
    pub actor_id: Option<String>,
}

/// Input for an Operator Manual Retry of failed Delivery.
#[derive(Debug, Clone)]
pub struct ManualRetryDelivery {
    /// The `delivery_failed` Recording to retry.
    pub recording_id: String,
    /// Stable id for the appended audit event.
    pub audit_event_id: String,
    /// When the retry was requested.
    pub at: OffsetDateTime,
    /// Fresh deadline after which Delivery retries stop again.
    pub retry_deadline_at: OffsetDateTime,
    /// Operator identifier for the audit event, if known.
    pub actor_id: Option<String>,
}

/// Which Recordings the monitor list should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatusFilter {
    /// All recent Recordings.
    All,
    /// Only `backlogged`.
    Backlogged,
    /// `transcription_failed` and `delivery_failed`.
    Failed,
    /// In-flight work: `transcribing`, `routing`, and `delivering`.
    Delivering,
    /// Only `delivered`.
    Delivered,
}

impl MonitorStatusFilter {
    /// The Recording statuses this filter selects, or `None` for no restriction.
    pub fn statuses(self) -> Option<&'static [RecordingStatus]> {
        use RecordingStatus::*;
        match self {
            MonitorStatusFilter::All => None,
            MonitorStatusFilter::Backlogged => Some(&[Backlogged]),
            MonitorStatusFilter::Failed => Some(&[TranscriptionFailed, DeliveryFailed]),
            MonitorStatusFilter::Delivering => Some(&[Transcribing, Routing, Delivering]),
            MonitorStatusFilter::Delivered => Some(&[Delivered]),
        }
    }
}

/// A compact Recording row for the monitor list page.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingSummary {
    /// Server Recording id.
    pub id: String,
    /// Submitting Client.
    pub client_id: String,
    /// Client-assigned Recording id.
    pub client_recording_id: String,
    /// Coarse lifecycle status.
    pub status: RecordingStatus,
    /// Normalized tags.
    pub tags: Tags,
    /// Selected Sink name, if any.
    pub selected_sink_name: Option<String>,
    /// Most recent error, if any.
    pub latest_error: Option<String>,
    /// Server receive time.
    pub received_at: OffsetDateTime,
}

/// The full read model for a Recording detail page.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingDetail {
    /// The Recording itself.
    pub recording: Recording,
    /// Its Transcript, when transcription has succeeded.
    pub transcript: Option<Transcript>,
    /// Its Delivery, when a Sink was selected.
    pub delivery: Option<Delivery>,
    /// Transcription Attempts, oldest first.
    pub transcription_attempts: Vec<TranscriptionAttempt>,
    /// Delivery Attempts, oldest first.
    pub delivery_attempts: Vec<DeliveryAttempt>,
    /// Audit events for the Recording, oldest first.
    pub audit_events: Vec<AuditEvent>,
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

/// A `transcribing` Recording eligible for a retry attempt: it has at least one
/// finished Transcription Attempt and no in-flight one.
#[derive(Debug, Clone)]
pub struct TranscriptionRetryCandidate {
    /// The Recording awaiting another Transcription attempt.
    pub recording: Recording,
    /// Number of the latest (finished) Transcription Attempt.
    pub last_attempt_number: i64,
    /// When the latest Transcription Attempt finished. Backoff is measured from
    /// the failure, not from when the attempt started.
    pub last_attempt_finished_at: OffsetDateTime,
    /// When an Operator last requested a manual retry, if any. When this is after
    /// `last_attempt_finished_at`, the work is due immediately (backoff bypassed).
    pub retry_window_started_at: Option<OffsetDateTime>,
}

/// The result of claiming an in-flight Transcription Attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionAttemptStart {
    /// 1-based number assigned to the new attempt.
    pub attempt_number: i64,
    /// When the first Transcription Attempt for this Recording started, used to
    /// anchor the retry deadline window.
    pub first_started_at: OffsetDateTime,
}

/// A `delivering` Delivery eligible for a Delivery attempt: it has no in-flight
/// attempt. `last_attempt_*` is `None` before the first attempt.
#[derive(Debug, Clone)]
pub struct DeliveryCandidate {
    /// The Delivery to attempt.
    pub delivery: Delivery,
    /// The Recording being delivered.
    pub recording: Recording,
    /// The Transcript payload to deliver.
    pub transcript: Transcript,
    /// Number of the latest finished Delivery Attempt, if any.
    pub last_attempt_number: Option<i64>,
    /// When the latest finished Delivery Attempt finished, if any. Backoff is
    /// measured from the failure, not from when the attempt started.
    pub last_attempt_finished_at: Option<OffsetDateTime>,
    /// When an Operator last requested a manual retry, if any. When this is after
    /// `last_attempt_finished_at`, the work is due immediately (backoff bypassed).
    pub retry_window_started_at: Option<OffsetDateTime>,
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
