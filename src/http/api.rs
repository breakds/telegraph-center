//! The `POST /api/recordings` upload handler.

use std::path::Path;

use axum::Json;
use axum::extract::State;
use axum::extract::multipart::{Field, Multipart};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use time::{Duration, OffsetDateTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::blob::BlobStore;
use crate::domain::{Recording, Tags, timestamp};
use crate::storage::NewRecording;
use crate::wav;

use super::AppState;
use super::error::ApiError;

const FINGERPRINT_HEADER: &str = "x-telegraph-client-fingerprint";
const RECORDING_ID_HEADER: &str = "x-telegraph-client-recording-id";

/// Maximum future skew allowed for a Client-provided `recorded_at`.
const MAX_RECORDED_AT_SKEW: Duration = Duration::hours(24);

/// How many leading bytes of the uploaded file to read for WAV header parsing.
const HEADER_SCAN_BYTES: usize = 64 * 1024;

/// Upper bound on a buffered text field (`tags`, `recorded_at`). These are
/// always small in legitimate use; this caps memory for a single field
/// independently of the overall upload budget.
const MAX_TEXT_FIELD_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct UploadResponse {
    recording_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate: Option<bool>,
}

/// Handle a Recording upload.
pub async fn upload_recording(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    let fingerprint =
        trimmed_header(&headers, FINGERPRINT_HEADER).ok_or(ApiError::MissingFingerprint)?;
    let client = state
        .config
        .client_by_fingerprint(&fingerprint)
        .ok_or(ApiError::UnknownClient)?;
    let client_recording_id =
        trimmed_header(&headers, RECORDING_ID_HEADER).ok_or(ApiError::MissingRecordingId)?;

    // Short-circuit a known duplicate before reading the body so we neither
    // store the audio again nor overwrite the existing blob.
    if let Some(existing) = state
        .store
        .get_recording_by_idempotency(&client.name, &client_recording_id)
        .await?
    {
        return Ok(duplicate_response(existing));
    }

    let recording_id = state.ids.generate();
    let received_at = state.clock.now();
    let temp_path = state.blobs.temp_path(&recording_id);

    let result = ingest(
        &state,
        &client.name,
        &client_recording_id,
        &recording_id,
        received_at,
        &temp_path,
        multipart,
    )
    .await;

    if result.is_err() {
        // Any partially written temp file is abandoned; remove it. The final
        // blob, if it was already renamed into place, is cleaned up inside
        // `ingest` on the error paths that can reach that point.
        state.blobs.remove_temp(&temp_path).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn ingest(
    state: &AppState,
    client_id: &str,
    client_recording_id: &str,
    recording_id: &str,
    received_at: OffsetDateTime,
    temp_path: &Path,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    let mut audio_written = false;
    let mut original_filename = None;
    let mut tags_raw: Option<String> = None;
    let mut recorded_at_raw: Option<String> = None;

    // One byte budget across the whole body. Every field (audio, text, and
    // ignored unknown fields) is counted against it, so a large non-audio
    // field cannot bypass the configured limit. The default body limit is
    // disabled at the router, so this is the only size guard.
    let mut remaining = state.config.data.max_upload_bytes;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::InvalidMultipart)?
    {
        match field.name() {
            Some("audio") => {
                if audio_written {
                    return Err(ApiError::MultipleAudio);
                }
                original_filename = field.file_name().map(str::to_string);
                stream_to_file(&mut field, temp_path, &mut remaining).await?;
                audio_written = true;
            }
            Some("tags") => {
                tags_raw = Some(read_text_field(&mut field, &mut remaining).await?);
            }
            Some("recorded_at") => {
                recorded_at_raw = Some(read_text_field(&mut field, &mut remaining).await?);
            }
            // Unknown fields are ignored, but still drained against the budget
            // so they cannot be used to exceed the limit.
            _ => discard_field(&mut field, &mut remaining).await?,
        }
    }

    if !audio_written {
        return Err(ApiError::MissingAudio);
    }

    let tags = parse_tags(tags_raw.as_deref())?;
    let recorded_at = parse_recorded_at(recorded_at_raw.as_deref(), received_at)?;

    // Parse WAV metadata from the leading bytes; the full payload is on disk.
    let header = read_leading_bytes(temp_path, HEADER_SCAN_BYTES).await?;
    let info = wav::parse_header(&header).map_err(|_| ApiError::UnsupportedAudio)?;
    let audio_size_bytes = tokio::fs::metadata(temp_path).await?.len() as i64;

    // Finalize the blob, then create the row. This ordering favors at most an
    // orphaned final blob (cleaned up below on insert failure) over a DB row
    // that points at a missing file.
    let relative = BlobStore::relative_path(recording_id);
    state.blobs.finalize(temp_path, &relative).await?;

    let new = NewRecording {
        id: recording_id.to_string(),
        client_id: client_id.to_string(),
        client_recording_id: client_recording_id.to_string(),
        original_filename,
        blob_path: Some(relative.clone()),
        audio_size_bytes: Some(audio_size_bytes),
        audio_duration_ms: info.duration_ms(),
        sample_rate_hz: Some(i64::from(info.sample_rate_hz)),
        channels: Some(i64::from(info.channels)),
        bits_per_sample: Some(i64::from(info.bits_per_sample)),
        tags,
        recorded_at,
        received_at,
    };

    let creation = match state.store.create_recording(new).await {
        Ok(creation) => creation,
        Err(err) => {
            state.blobs.remove_blob(&relative).await;
            return Err(err.into());
        }
    };

    if creation.created {
        Ok(new_response(creation.recording))
    } else {
        // Lost an idempotency race after we wrote our blob: ours is an orphan.
        state.blobs.remove_blob(&relative).await;
        Ok(duplicate_response(creation.recording))
    }
}

/// Subtract `len` from the remaining byte budget, failing if it is exhausted.
fn charge(remaining: &mut u64, len: usize) -> Result<(), ApiError> {
    match remaining.checked_sub(len as u64) {
        Some(left) => {
            *remaining = left;
            Ok(())
        }
        None => Err(ApiError::PayloadTooLarge),
    }
}

/// Stream a multipart field to a file, charging the shared byte budget as it
/// goes so the limit is enforced without buffering the whole body.
async fn stream_to_file(
    field: &mut Field<'_>,
    path: &Path,
    remaining: &mut u64,
) -> Result<(), ApiError> {
    let mut file = tokio::fs::File::create(path).await?;

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| ApiError::InvalidMultipart)?
    {
        charge(remaining, chunk.len())?;
        file.write_all(chunk.as_ref()).await?;
    }

    file.flush().await?;
    Ok(())
}

/// Read a small text field into memory, charging the shared byte budget and
/// enforcing a tight per-field cap so a non-audio field cannot be used to
/// buffer large amounts of memory.
async fn read_text_field(field: &mut Field<'_>, remaining: &mut u64) -> Result<String, ApiError> {
    let mut buffer = Vec::new();

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| ApiError::InvalidMultipart)?
    {
        charge(remaining, chunk.len())?;
        if buffer.len() + chunk.len() > MAX_TEXT_FIELD_BYTES {
            return Err(ApiError::PayloadTooLarge);
        }
        buffer.extend_from_slice(chunk.as_ref());
    }

    String::from_utf8(buffer).map_err(|_| ApiError::InvalidMultipart)
}

/// Drain an ignored field, charging the shared byte budget without buffering.
async fn discard_field(field: &mut Field<'_>, remaining: &mut u64) -> Result<(), ApiError> {
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| ApiError::InvalidMultipart)?
    {
        charge(remaining, chunk.len())?;
    }
    Ok(())
}

async fn read_leading_bytes(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0u8; max];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

fn parse_tags(raw: Option<&str>) -> Result<Tags, ApiError> {
    match raw {
        Some(raw) => {
            let values: Vec<String> =
                serde_json::from_str(raw).map_err(|_| ApiError::InvalidTags)?;
            Tags::new(values).map_err(|_| ApiError::InvalidTags)
        }
        None => Ok(Tags::default()),
    }
}

fn parse_recorded_at(
    raw: Option<&str>,
    received_at: OffsetDateTime,
) -> Result<Option<OffsetDateTime>, ApiError> {
    match raw {
        Some(raw) => {
            let recorded = timestamp::parse(raw.trim()).map_err(|_| ApiError::InvalidRecordedAt)?;
            if recorded > received_at + MAX_RECORDED_AT_SKEW {
                return Err(ApiError::RecordedAtInFuture);
            }
            Ok(Some(recorded))
        }
        None => Ok(None),
    }
}

/// Read a header value, returning `None` if absent, non-ASCII, or blank.
fn trimmed_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn new_response(recording: Recording) -> Response {
    (
        StatusCode::CREATED,
        Json(UploadResponse {
            recording_id: recording.id,
            status: recording.status.as_str().to_string(),
            duplicate: None,
        }),
    )
        .into_response()
}

fn duplicate_response(recording: Recording) -> Response {
    (
        StatusCode::OK,
        Json(UploadResponse {
            recording_id: recording.id,
            status: recording.status.as_str().to_string(),
            duplicate: Some(true),
        }),
    )
        .into_response()
}
