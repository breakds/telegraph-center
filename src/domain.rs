//! Core domain types for Telegraph Center.
//!
//! These types model Recordings and the state they move through, plus the
//! supporting concepts (Transcripts, Sinks, Deliveries, Attempts, Operator
//! Sessions). They carry no persistence or transport concerns so they can be
//! exercised in isolation. Names follow `TERMINOLOGY.md`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// RFC3339 conversion helpers for the timestamp text stored in SQLite.
pub mod timestamp {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    /// Render a timestamp as RFC3339 text.
    pub fn format(value: OffsetDateTime) -> String {
        value
            .format(&Rfc3339)
            .expect("OffsetDateTime always formats as RFC3339")
    }

    /// Parse RFC3339 text back into a timestamp.
    pub fn parse(value: &str) -> Result<OffsetDateTime, time::error::Parse> {
        OffsetDateTime::parse(value, &Rfc3339)
    }
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// Maximum number of tags allowed on a Recording or Sink rule.
pub const MAX_TAGS: usize = 16;

/// Longest permitted tag length, including the leading character.
pub const MAX_TAG_LEN: usize = 32;

/// A reason a set of tags failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// More than `MAX_TAGS` tags were supplied after normalization.
    #[error("too many tags: {count} (max {max})")]
    TooMany {
        /// Number of tags after normalization.
        count: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// A tag does not match the permitted shape.
    #[error("invalid tag: {tag:?}")]
    Invalid {
        /// The offending tag, after normalization.
        tag: String,
    },
}

/// A normalized, validated set of tags.
///
/// Construction trims surrounding whitespace, lowercases ASCII, and
/// deduplicates while preserving first-seen order. It never silently rewrites
/// internal spaces or punctuation; such tags are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tags(Vec<String>);

impl Tags {
    /// Normalize and validate raw tag strings.
    pub fn new<I, S>(raw: I) -> Result<Self, TagError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut normalized: Vec<String> = Vec::new();
        for item in raw {
            let tag = item.as_ref().trim().to_ascii_lowercase();
            if !normalized.contains(&tag) {
                normalized.push(tag);
            }
        }

        if normalized.len() > MAX_TAGS {
            return Err(TagError::TooMany {
                count: normalized.len(),
                max: MAX_TAGS,
            });
        }

        for tag in &normalized {
            if !is_valid_tag(tag) {
                return Err(TagError::Invalid { tag: tag.clone() });
            }
        }

        Ok(Self(normalized))
    }

    /// Wrap tag values already known to be normalized (for example, read back
    /// from storage). No validation is performed.
    pub fn from_stored(values: Vec<String>) -> Self {
        Self(values)
    }

    /// Serialize to the JSON text form persisted in SQLite.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).expect("Vec<String> always serializes")
    }

    /// Parse the JSON text form persisted in SQLite.
    pub fn from_json(value: &str) -> Result<Self, serde_json::Error> {
        Ok(Self(serde_json::from_str(value)?))
    }

    /// The tags as a slice.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Whether there are no tags.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of tags.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether any tag is shared with `other`.
    pub fn intersects(&self, other: &Tags) -> bool {
        self.0.iter().any(|tag| other.0.contains(tag))
    }
}

fn is_valid_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_TAG_LEN {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

// ---------------------------------------------------------------------------
// Recording status
// ---------------------------------------------------------------------------

/// The coarse lifecycle state of a Recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    /// Upload is durably stored and waiting for Transcription.
    Received,
    /// Transcription is in progress or retryable.
    Transcribing,
    /// A Transcript exists and routing is being evaluated.
    Routing,
    /// No Sink was selected by Routing Rules; awaiting Manual Routing.
    Backlogged,
    /// A Sink was selected and Delivery is in progress or retryable.
    Delivering,
    /// The selected Sink accepted the Transcript payload.
    Delivered,
    /// Transcription reached a terminal error or its retry deadline.
    TranscriptionFailed,
    /// Delivery reached its retry deadline.
    DeliveryFailed,
}

/// Returned when a string cannot be mapped to a `RecordingStatus`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown recording status: {0:?}")]
pub struct ParseRecordingStatusError(pub String);

/// Returned when a Recording status transition is not permitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid recording status transition from {from} to {to}")]
pub struct InvalidTransition {
    /// The current status.
    pub from: RecordingStatus,
    /// The rejected target status.
    pub to: RecordingStatus,
}

impl RecordingStatus {
    /// The stored text form.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordingStatus::Received => "received",
            RecordingStatus::Transcribing => "transcribing",
            RecordingStatus::Routing => "routing",
            RecordingStatus::Backlogged => "backlogged",
            RecordingStatus::Delivering => "delivering",
            RecordingStatus::Delivered => "delivered",
            RecordingStatus::TranscriptionFailed => "transcription_failed",
            RecordingStatus::DeliveryFailed => "delivery_failed",
        }
    }

    /// Parse the stored text form.
    pub fn parse(value: &str) -> Result<Self, ParseRecordingStatusError> {
        let status = match value {
            "received" => RecordingStatus::Received,
            "transcribing" => RecordingStatus::Transcribing,
            "routing" => RecordingStatus::Routing,
            "backlogged" => RecordingStatus::Backlogged,
            "delivering" => RecordingStatus::Delivering,
            "delivered" => RecordingStatus::Delivered,
            "transcription_failed" => RecordingStatus::TranscriptionFailed,
            "delivery_failed" => RecordingStatus::DeliveryFailed,
            other => return Err(ParseRecordingStatusError(other.to_string())),
        };
        Ok(status)
    }

    /// Whether `self -> next` is an allowed transition.
    ///
    /// Forward progress follows the documented lifecycle. Manual recovery
    /// transitions (retry after failure, Manual Routing out of the Backlog)
    /// re-enter the active states. Backlog is reachable only from `routing`,
    /// keeping it distinct from a failed Delivery to an already selected Sink.
    pub fn can_transition_to(self, next: RecordingStatus) -> bool {
        use RecordingStatus::*;
        matches!(
            (self, next),
            (Received, Transcribing)
                | (Transcribing, Routing)
                | (Transcribing, TranscriptionFailed)
                | (TranscriptionFailed, Transcribing)
                | (Routing, Backlogged)
                | (Routing, Delivering)
                | (Backlogged, Delivering)
                | (Delivering, Delivered)
                | (Delivering, DeliveryFailed)
                | (DeliveryFailed, Delivering)
        )
    }

    /// Validate a transition, returning an error if it is not permitted.
    pub fn transition_to(self, next: RecordingStatus) -> Result<(), InvalidTransition> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

impl std::fmt::Display for RecordingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Delivery status
// ---------------------------------------------------------------------------

/// The state of a Delivery to a selected Sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Delivery is in progress or retryable.
    Delivering,
    /// The Sink accepted the payload.
    Delivered,
    /// Delivery reached its retry deadline.
    DeliveryFailed,
}

/// Returned when a string cannot be mapped to a `DeliveryStatus`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown delivery status: {0:?}")]
pub struct ParseDeliveryStatusError(pub String);

impl DeliveryStatus {
    /// The stored text form.
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Delivering => "delivering",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::DeliveryFailed => "delivery_failed",
        }
    }

    /// Parse the stored text form.
    pub fn parse(value: &str) -> Result<Self, ParseDeliveryStatusError> {
        let status = match value {
            "delivering" => DeliveryStatus::Delivering,
            "delivered" => DeliveryStatus::Delivered,
            "delivery_failed" => DeliveryStatus::DeliveryFailed,
            other => return Err(ParseDeliveryStatusError(other.to_string())),
        };
        Ok(status)
    }
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Recording and related records
// ---------------------------------------------------------------------------

/// A completed audio capture tracked through transcription and delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    /// Server-generated stable identifier.
    pub id: String,
    /// Configured Client that submitted the Recording.
    pub client_id: String,
    /// Client-assigned identifier, unique within the Client.
    pub client_recording_id: String,
    /// Coarse lifecycle state.
    pub status: RecordingStatus,
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
    /// Server receive time.
    pub received_at: OffsetDateTime,
    /// Name of the Sink selected for Delivery, if any.
    pub selected_sink_name: Option<String>,
    /// Most recent error surfaced for the Recording.
    pub latest_error: Option<String>,
    /// Row creation time.
    pub created_at: OffsetDateTime,
    /// Last update time.
    pub updated_at: OffsetDateTime,
}

/// The text and provider artifacts derived from a Recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    /// The Recording this Transcript belongs to.
    pub recording_id: String,
    /// Speech-to-text provider name.
    pub provider: String,
    /// Rendered plain-text Transcript.
    pub text: String,
    /// Raw provider JSON, kept for debugging and future re-rendering.
    pub raw_json: String,
    /// Provider-side file identifier, if any.
    pub provider_file_id: Option<String>,
    /// Provider-side transcription identifier, if any.
    pub provider_transcription_id: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// One try at producing a Transcript for a Recording.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionAttempt {
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

/// The logical act of sending a Transcript payload to a selected Sink.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// Stable Delivery identifier, reused across HTTP retries.
    pub id: String,
    /// The Recording being delivered.
    pub recording_id: String,
    /// Name of the selected Sink.
    pub sink_name: String,
    /// Delivery state.
    pub status: DeliveryStatus,
    /// When the Sink was selected.
    pub selected_at: OffsetDateTime,
    /// When the Delivery reached a terminal state, if it did.
    pub completed_at: Option<OffsetDateTime>,
    /// Deadline after which retries stop.
    pub retry_deadline_at: Option<OffsetDateTime>,
    /// Most recent Delivery error.
    pub latest_error: Option<String>,
}

/// One try at completing a Delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryAttempt {
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

/// An authenticated browser session for an Operator.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorSession {
    /// Hash of the session token (the token itself is never stored).
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
    /// When the session was revoked, if it was.
    pub revoked_at: Option<OffsetDateTime>,
}

/// An append-only record of a noteworthy action.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_normalize_trim_lowercase_dedup_preserving_order() {
        let tags = Tags::new([" Journal ", "DIARY", "journal"]).unwrap();
        assert_eq!(
            tags.as_slice(),
            &["journal".to_string(), "diary".to_string()]
        );
    }

    #[test]
    fn tags_reject_internal_spaces() {
        let err = Tags::new(["to do"]).unwrap_err();
        assert_eq!(
            err,
            TagError::Invalid {
                tag: "to do".into()
            }
        );
    }

    #[test]
    fn tags_reject_disallowed_punctuation() {
        assert!(matches!(
            Tags::new(["hello!"]).unwrap_err(),
            TagError::Invalid { .. }
        ));
    }

    #[test]
    fn tags_reject_leading_separator() {
        assert!(matches!(
            Tags::new(["-bad"]).unwrap_err(),
            TagError::Invalid { .. }
        ));
    }

    #[test]
    fn tags_allow_digits_underscores_and_hyphens() {
        let tags = Tags::new(["v1", "a_b-c", "0"]).unwrap();
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn tags_reject_too_many() {
        let raw: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        assert_eq!(
            Tags::new(raw).unwrap_err(),
            TagError::TooMany { count: 17, max: 16 }
        );
    }

    #[test]
    fn tags_reject_too_long() {
        let long = "a".repeat(33);
        assert!(matches!(
            Tags::new([long]).unwrap_err(),
            TagError::Invalid { .. }
        ));
    }

    #[test]
    fn tags_json_round_trips() {
        let tags = Tags::new(["journal", "diary"]).unwrap();
        let restored = Tags::from_json(&tags.to_json()).unwrap();
        assert_eq!(tags, restored);
    }

    #[test]
    fn tags_intersection() {
        let a = Tags::new(["journal", "diary"]).unwrap();
        let b = Tags::new(["todo", "diary"]).unwrap();
        let c = Tags::new(["todo"]).unwrap();
        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));
    }

    #[test]
    fn status_text_round_trips() {
        for status in [
            RecordingStatus::Received,
            RecordingStatus::Transcribing,
            RecordingStatus::Routing,
            RecordingStatus::Backlogged,
            RecordingStatus::Delivering,
            RecordingStatus::Delivered,
            RecordingStatus::TranscriptionFailed,
            RecordingStatus::DeliveryFailed,
        ] {
            assert_eq!(RecordingStatus::parse(status.as_str()).unwrap(), status);
        }
    }

    #[test]
    fn status_allows_documented_transitions() {
        use RecordingStatus::*;
        let allowed = [
            (Received, Transcribing),
            (Transcribing, Routing),
            (Routing, Backlogged),
            (Routing, Delivering),
            (Delivering, Delivered),
            (Delivering, DeliveryFailed),
            (Transcribing, TranscriptionFailed),
        ];
        for (from, to) in allowed {
            assert!(
                from.can_transition_to(to),
                "{from} -> {to} should be allowed"
            );
        }
    }

    #[test]
    fn status_rejects_nonsensical_transitions() {
        use RecordingStatus::*;
        assert!(!Delivered.can_transition_to(Received));
        assert!(!Received.can_transition_to(Delivered));
        assert!(!Backlogged.can_transition_to(DeliveryFailed));
        assert_eq!(
            Delivered.transition_to(Received).unwrap_err(),
            InvalidTransition {
                from: Delivered,
                to: Received
            }
        );
    }
}
