//! Delivery worker tick.

use time::OffsetDateTime;

use crate::seam::Clock;
use crate::storage::{DeliveryCandidate, StorageError};

use super::{DeliveryRequest, SinkClient, WorkOutcome, WorkerContext, WorkerFailure, retry};

/// Process one due Delivery, if any is ready.
///
/// Claims a `delivering` Delivery whose backoff is due (or its first attempt),
/// records an attempt, calls the `sink`, and marks the Delivery delivered,
/// retryable, or failed. The clock is read before the attempt and again after
/// the `sink` returns, so backoff is measured from the failure. A retryable
/// failure past the 24-hour deadline becomes a terminal failure
/// (`delivery_failed`), which is distinct from the Backlog.
pub async fn tick_once<S: SinkClient>(
    ctx: &WorkerContext<'_>,
    sink: &S,
    clock: &dyn Clock,
) -> Result<WorkOutcome, StorageError> {
    let started_at = clock.now();

    let Some(candidate) = pick_due_delivery(ctx, started_at).await? else {
        return Ok(WorkOutcome::Idle);
    };

    let attempt_id = ctx.ids.generate();
    let Some(_attempt_number) = ctx
        .store
        .start_delivery_attempt(&attempt_id, &candidate.delivery.id, started_at)
        .await?
    else {
        // Another worker claimed it first.
        return Ok(WorkOutcome::Idle);
    };

    let request = DeliveryRequest {
        recording: &candidate.recording,
        delivery: &candidate.delivery,
        transcript: &candidate.transcript,
    };

    let result = sink.deliver(request).await;
    let finished_at = clock.now();

    match result {
        Ok(()) => {
            ctx.store
                .succeed_delivery(&attempt_id, &candidate.delivery.id, finished_at, None)
                .await?;
        }
        Err(WorkerFailure::Retryable { message, .. }) => {
            let expired = candidate
                .delivery
                .retry_deadline_at
                .is_some_and(|deadline| finished_at >= deadline);
            if expired {
                ctx.store
                    .fail_delivery(
                        &attempt_id,
                        &candidate.delivery.id,
                        finished_at,
                        None,
                        &message,
                    )
                    .await?;
            } else {
                ctx.store
                    .retry_delivery(
                        &attempt_id,
                        &candidate.delivery.id,
                        finished_at,
                        None,
                        &message,
                    )
                    .await?;
            }
        }
        Err(WorkerFailure::Terminal { message, .. }) => {
            ctx.store
                .fail_delivery(
                    &attempt_id,
                    &candidate.delivery.id,
                    finished_at,
                    None,
                    &message,
                )
                .await?;
        }
    }

    Ok(WorkOutcome::Worked)
}

/// The oldest `delivering` Delivery that is due: its first attempt (due
/// immediately), a retry whose backoff has elapsed since the last failure, or an
/// Operator-opened fresh retry window after the last attempt (Manual Retry),
/// which is due immediately.
async fn pick_due_delivery(
    ctx: &WorkerContext<'_>,
    now: OffsetDateTime,
) -> Result<Option<DeliveryCandidate>, StorageError> {
    for candidate in ctx.store.delivery_candidates().await? {
        let manual_retry = candidate.retry_window_started_at.is_some_and(|window| {
            candidate
                .last_attempt_finished_at
                .is_none_or(|finished_at| window > finished_at)
        });
        let due = match (
            candidate.last_attempt_number,
            candidate.last_attempt_finished_at,
        ) {
            (Some(number), Some(finished_at)) => retry::next_retry_at(finished_at, number),
            // No attempts yet: the first attempt is due as soon as the Sink was
            // selected.
            _ => candidate.delivery.selected_at,
        };
        if manual_retry || now >= due {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
