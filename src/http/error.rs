//! Stable error response shape for the Client API.
//!
//! Every error renders as `{ "error": <code>, "message": <text> }`. Messages
//! are deliberately generic and never include filesystem paths or secrets.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::storage::StorageError;

/// An error returned by the upload handler.
#[derive(Debug)]
pub enum ApiError {
    /// No Client identity header was present.
    MissingFingerprint,
    /// The Client identity did not match any configured Client.
    UnknownClient,
    /// The idempotency-key header was missing or empty.
    MissingRecordingId,
    /// The request had no `audio` field.
    MissingAudio,
    /// The request had more than one `audio` field.
    MultipleAudio,
    /// The `tags` field was not a valid JSON array of valid tags.
    InvalidTags,
    /// The `recorded_at` field was not a valid RFC3339 timestamp.
    InvalidRecordedAt,
    /// The `recorded_at` value was too far in the future.
    RecordedAtInFuture,
    /// The upload exceeded the configured size limit.
    PayloadTooLarge,
    /// The audio payload was not a supported WAV file.
    UnsupportedAudio,
    /// The multipart body could not be parsed.
    InvalidMultipart,
    /// An unexpected storage or filesystem failure occurred. The detail is for
    /// server-side logging only and is never sent to the Client.
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: &'a str,
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::MissingFingerprint => StatusCode::UNAUTHORIZED,
            ApiError::UnknownClient => StatusCode::FORBIDDEN,
            ApiError::MissingRecordingId
            | ApiError::MissingAudio
            | ApiError::MultipleAudio
            | ApiError::InvalidTags
            | ApiError::InvalidRecordedAt
            | ApiError::RecordedAtInFuture
            | ApiError::InvalidMultipart => StatusCode::BAD_REQUEST,
            ApiError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::UnsupportedAudio => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable machine-readable error code.
    fn code(&self) -> &'static str {
        match self {
            ApiError::MissingFingerprint => "missing_client_identity",
            ApiError::UnknownClient => "unknown_client",
            ApiError::MissingRecordingId => "missing_recording_id",
            ApiError::MissingAudio => "missing_audio",
            ApiError::MultipleAudio => "multiple_audio",
            ApiError::InvalidTags => "invalid_tags",
            ApiError::InvalidRecordedAt => "invalid_recorded_at",
            ApiError::RecordedAtInFuture => "recorded_at_in_future",
            ApiError::PayloadTooLarge => "payload_too_large",
            ApiError::UnsupportedAudio => "unsupported_audio",
            ApiError::InvalidMultipart => "invalid_multipart",
            ApiError::Internal(_) => "internal_error",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            ApiError::MissingFingerprint => "Missing client identity.",
            ApiError::UnknownClient => "Client is not authorized.",
            ApiError::MissingRecordingId => "Missing or empty client recording id.",
            ApiError::MissingAudio => "Missing required audio field.",
            ApiError::MultipleAudio => "Only one audio field is allowed.",
            ApiError::InvalidTags => "Tags must be a JSON array of valid tag strings.",
            ApiError::InvalidRecordedAt => "recorded_at must be an RFC3339 timestamp.",
            ApiError::RecordedAtInFuture => "recorded_at is too far in the future.",
            ApiError::PayloadTooLarge => "Upload exceeds the maximum allowed size.",
            ApiError::UnsupportedAudio => "Audio must be a WAV file.",
            ApiError::InvalidMultipart => "Request body is not valid multipart form data.",
            ApiError::Internal(_) => "An unexpected error occurred.",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(detail) = &self {
            // Surface the cause server-side without leaking it to the Client.
            eprintln!("upload internal error: {detail}");
        }
        let body = ErrorBody {
            error: self.code(),
            message: self.message(),
        };
        (self.status(), Json(body)).into_response()
    }
}

impl From<StorageError> for ApiError {
    fn from(err: StorageError) -> Self {
        ApiError::Internal(format!("storage: {err}"))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        ApiError::Internal(format!("io: {err}"))
    }
}
