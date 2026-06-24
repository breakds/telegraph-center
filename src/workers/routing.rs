//! Routing worker tick.

use crate::seam::Clock;
use crate::storage::{NewDelivery, StorageError};

use super::{Router, RoutingDecision, WorkOutcome, WorkerContext, retry};

/// Process one `routing` Recording, if any is ready.
///
/// Calls the `router`: a Sink decision selects the Sink and creates a Delivery
/// (moving the Recording to `delivering`); no decision moves the Recording to
/// the Backlog. Routing is a pure decision with no I/O, so the clock is read
/// once. A concurrent worker that already handled the Recording is tolerated
/// (the guarded transition is a no-op).
pub async fn tick_once<R: Router>(
    ctx: &WorkerContext<'_>,
    router: &R,
    clock: &dyn Clock,
) -> Result<WorkOutcome, StorageError> {
    let now = clock.now();

    let Some(recording) = ctx.store.claim_routing_candidate().await? else {
        return Ok(WorkOutcome::Idle);
    };

    match router.route(&recording) {
        RoutingDecision::Backlog => match ctx.store.mark_backlogged(&recording.id, now).await {
            Ok(_) | Err(StorageError::InvalidTransition(_)) => {}
            Err(err) => return Err(err),
        },
        RoutingDecision::Sink(sink_name) => {
            let delivery = NewDelivery {
                id: ctx.ids.generate(),
                recording_id: recording.id.clone(),
                sink_name,
                selected_at: now,
                retry_deadline_at: Some(now + retry::DELIVERY_WINDOW),
            };
            // `route_to_sink` is race-safe: a concurrent worker that already
            // handled this Recording yields `AlreadyHandled` rather than an error.
            ctx.store.route_to_sink(delivery).await?;
        }
    }

    Ok(WorkOutcome::Worked)
}
