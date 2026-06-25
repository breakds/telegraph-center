//! Server-rendered monitor pages (ADR 0010).
//!
//! Templates live under `templates/monitor/` and share a base layout. Each
//! template struct is a small view model with pre-formatted strings, so the
//! templates stay free of date formatting and option juggling and Askama can
//! HTML-escape every interpolated value.

use askama::Template;
use time::OffsetDateTime;

use crate::domain::timestamp;
use crate::storage::{RecordingDetail, RecordingSummary};

/// The login page. `error` is empty when there is nothing to report.
#[derive(Template)]
#[template(path = "monitor/login.html")]
pub struct LoginTemplate {
    /// A generic error message to show, or empty for none.
    pub error: String,
}

/// The Recording list page.
#[derive(Template)]
#[template(path = "monitor/list.html")]
pub struct RecordingListTemplate {
    /// Raw CSRF token for the logout form in the nav.
    pub csrf_token: String,
    /// The currently active status filter label, for nav highlighting.
    pub active_filter: &'static str,
    /// Recording rows, newest first.
    pub rows: Vec<SummaryView>,
}

/// One Recording row on the list page.
pub struct SummaryView {
    pub id: String,
    pub received_at: String,
    pub client_id: String,
    pub client_recording_id: String,
    pub status: String,
    pub tags: String,
    pub selected_sink_name: Option<String>,
    pub latest_error: Option<String>,
}

/// The Recording detail page.
#[derive(Template)]
#[template(path = "monitor/detail.html")]
pub struct RecordingDetailTemplate {
    /// Raw CSRF token for action forms and the logout form.
    pub csrf_token: String,
    /// Detail pages do not correspond to a list filter.
    pub active_filter: &'static str,
    /// Recording metadata.
    pub rec: DetailView,
    /// Transcript, when available.
    pub transcript: Option<TranscriptView>,
    /// Delivery, when a Sink was selected.
    pub delivery: Option<DeliveryView>,
    /// Transcription Attempts, oldest first.
    pub transcription_attempts: Vec<TranscriptionAttemptView>,
    /// Delivery Attempts, oldest first.
    pub delivery_attempts: Vec<DeliveryAttemptView>,
    /// Audit events, oldest first.
    pub audit_events: Vec<AuditView>,
    /// Configured Sink names offered for Manual Routing.
    pub sinks: Vec<String>,
    /// Whether the Manual Routing form should be shown.
    pub can_manual_route: bool,
    /// Whether the retry-transcription form should be shown.
    pub can_retry_transcription: bool,
    /// Whether the retry-delivery form should be shown.
    pub can_retry_delivery: bool,
}

pub struct DetailView {
    pub id: String,
    pub client_id: String,
    pub client_recording_id: String,
    pub status: String,
    pub original_filename: Option<String>,
    pub received_at: String,
    pub recorded_at: Option<String>,
    pub audio_size_bytes: Option<i64>,
    pub audio_duration_ms: Option<i64>,
    pub sample_rate_hz: Option<i64>,
    pub channels: Option<i64>,
    pub bits_per_sample: Option<i64>,
    pub tags: String,
    pub latest_error: Option<String>,
}

pub struct TranscriptView {
    pub provider: String,
    pub text: String,
    pub raw_json: String,
}

pub struct DeliveryView {
    pub sink_name: String,
    pub status: String,
    pub selected_at: String,
    pub completed_at: Option<String>,
    pub retry_deadline_at: Option<String>,
    pub latest_error: Option<String>,
}

pub struct TranscriptionAttemptView {
    pub attempt_number: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub retryable: bool,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

pub struct DeliveryAttemptView {
    pub attempt_number: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub http_status: Option<i64>,
    pub retryable: bool,
    pub error_message: Option<String>,
}

pub struct AuditView {
    pub occurred_at: String,
    pub event_type: String,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub details_json: String,
}

fn fmt_ts(value: OffsetDateTime) -> String {
    timestamp::format(value)
}

fn fmt_opt_ts(value: Option<OffsetDateTime>) -> Option<String> {
    value.map(timestamp::format)
}

fn join_tags(tags: &crate::domain::Tags) -> String {
    tags.as_slice().join(", ")
}

impl SummaryView {
    /// Build a list row from a repository summary.
    pub fn from_summary(summary: RecordingSummary) -> Self {
        Self {
            id: summary.id,
            received_at: fmt_ts(summary.received_at),
            client_id: summary.client_id,
            client_recording_id: summary.client_recording_id,
            status: summary.status.to_string(),
            tags: join_tags(&summary.tags),
            selected_sink_name: summary.selected_sink_name,
            latest_error: summary.latest_error,
        }
    }
}

impl RecordingDetailTemplate {
    /// Build the detail page from a repository read model, the session CSRF
    /// token, and the configured Sink names available for Manual Routing.
    pub fn from_detail(detail: RecordingDetail, csrf_token: String, sinks: Vec<String>) -> Self {
        use crate::domain::RecordingStatus;

        let status = detail.recording.status;
        let r = detail.recording;

        Self {
            csrf_token,
            active_filter: "",
            rec: DetailView {
                id: r.id,
                client_id: r.client_id,
                client_recording_id: r.client_recording_id,
                status: status.to_string(),
                original_filename: r.original_filename,
                received_at: fmt_ts(r.received_at),
                recorded_at: fmt_opt_ts(r.recorded_at),
                audio_size_bytes: r.audio_size_bytes,
                audio_duration_ms: r.audio_duration_ms,
                sample_rate_hz: r.sample_rate_hz,
                channels: r.channels,
                bits_per_sample: r.bits_per_sample,
                tags: join_tags(&r.tags),
                latest_error: r.latest_error,
            },
            transcript: detail.transcript.map(|t| TranscriptView {
                provider: t.provider,
                text: t.text,
                raw_json: t.raw_json,
            }),
            delivery: detail.delivery.map(|d| DeliveryView {
                sink_name: d.sink_name,
                status: d.status.to_string(),
                selected_at: fmt_ts(d.selected_at),
                completed_at: fmt_opt_ts(d.completed_at),
                retry_deadline_at: fmt_opt_ts(d.retry_deadline_at),
                latest_error: d.latest_error,
            }),
            transcription_attempts: detail
                .transcription_attempts
                .into_iter()
                .map(|a| TranscriptionAttemptView {
                    attempt_number: a.attempt_number,
                    started_at: fmt_ts(a.started_at),
                    finished_at: fmt_opt_ts(a.finished_at),
                    status: a.status,
                    retryable: a.retryable,
                    error_code: a.error_code,
                    error_message: a.error_message,
                })
                .collect(),
            delivery_attempts: detail
                .delivery_attempts
                .into_iter()
                .map(|a| DeliveryAttemptView {
                    attempt_number: a.attempt_number,
                    started_at: fmt_ts(a.started_at),
                    finished_at: fmt_opt_ts(a.finished_at),
                    status: a.status,
                    http_status: a.http_status,
                    retryable: a.retryable,
                    error_message: a.error_message,
                })
                .collect(),
            audit_events: detail
                .audit_events
                .into_iter()
                .map(|e| AuditView {
                    occurred_at: fmt_ts(e.occurred_at),
                    event_type: e.event_type,
                    actor_kind: e.actor_kind,
                    actor_id: e.actor_id,
                    details_json: e.details_json,
                })
                .collect(),
            sinks,
            can_manual_route: status == RecordingStatus::Backlogged,
            can_retry_transcription: status == RecordingStatus::TranscriptionFailed,
            can_retry_delivery: status == RecordingStatus::DeliveryFailed,
        }
    }
}
