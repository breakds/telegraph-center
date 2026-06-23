//! Transcription worker tick.

use time::OffsetDateTime;

use crate::domain::Recording;
use crate::seam::Clock;
use crate::storage::{NewTranscript, StorageError};

use super::{Transcriber, TranscriptionRequest, WorkOutcome, WorkerContext, WorkerFailure, retry};

/// Process one unit of Transcription work, if any is ready.
///
/// Claims either a `received` Recording (first attempt) or a due retryable
/// `transcribing` Recording, records an attempt, calls the `transcriber`, and
/// stores the Transcript or records the failure. The clock is read once before
/// the attempt (its start time) and again after the `transcriber` returns (its
/// finish time), so backoff is measured from when the attempt actually failed.
/// A retryable failure past the duration-aware deadline becomes terminal.
pub async fn tick_once<T: Transcriber>(
    ctx: &WorkerContext<'_>,
    transcriber: &T,
    clock: &dyn Clock,
) -> Result<WorkOutcome, StorageError> {
    let started_at = clock.now();

    let recording = match ctx
        .store
        .claim_received_for_transcription(started_at)
        .await?
    {
        Some(recording) => recording,
        None => match pick_due_retry(ctx, started_at).await? {
            Some(recording) => recording,
            None => return Ok(WorkOutcome::Idle),
        },
    };

    let attempt_id = ctx.ids.generate();
    let Some(start) = ctx
        .store
        .start_transcription_attempt(&attempt_id, &recording.id, started_at)
        .await?
    else {
        // Another worker claimed it first.
        return Ok(WorkOutcome::Idle);
    };

    let audio_path = recording
        .blob_path
        .as_deref()
        .map(|path| ctx.blobs.full_path(path))
        .unwrap_or_else(|| ctx.blobs.data_dir().to_path_buf());

    let request = TranscriptionRequest {
        recording: &recording,
        audio_path,
    };

    let result = transcriber.transcribe(request).await;
    let finished_at = clock.now();

    match result {
        Ok(output) => {
            let transcript = NewTranscript {
                recording_id: recording.id.clone(),
                provider: output.provider,
                text: output.text,
                raw_json: output.raw_json,
                provider_file_id: output.provider_file_id,
                provider_transcription_id: output.provider_transcription_id,
                created_at: finished_at,
            };
            ctx.store
                .succeed_transcription(&attempt_id, finished_at, transcript)
                .await?;
        }
        Err(WorkerFailure::Retryable { code, message }) => {
            let deadline =
                retry::transcription_deadline(start.first_started_at, recording.audio_duration_ms);
            if finished_at >= deadline {
                ctx.store
                    .fail_transcription(&attempt_id, &recording.id, finished_at, &code, &message)
                    .await?;
            } else {
                ctx.store
                    .retry_transcription(&attempt_id, &recording.id, finished_at, &code, &message)
                    .await?;
            }
        }
        Err(WorkerFailure::Terminal { code, message }) => {
            ctx.store
                .fail_transcription(&attempt_id, &recording.id, finished_at, &code, &message)
                .await?;
        }
    }

    Ok(WorkOutcome::Worked)
}

/// The oldest `transcribing` Recording whose latest attempt's backoff is due,
/// measured from that attempt's finish time.
async fn pick_due_retry(
    ctx: &WorkerContext<'_>,
    now: OffsetDateTime,
) -> Result<Option<Recording>, StorageError> {
    for candidate in ctx.store.transcription_retry_candidates().await? {
        let due = retry::next_retry_at(
            candidate.last_attempt_finished_at,
            candidate.last_attempt_number,
        );
        if now >= due {
            return Ok(Some(candidate.recording));
        }
    }
    Ok(None)
}
