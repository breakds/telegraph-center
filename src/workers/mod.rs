//! Background worker framework.
//!
//! Workers use SQLite as the work queue: status and attempt rows decide what is
//! ready to process. Each unit of work is a one-shot `tick_once` that claims a
//! row, calls an injected integration ([`Transcriber`], [`Router`],
//! [`SinkClient`]), and records the outcome. [`run_worker_loop`] drives a tick
//! on an interval until shutdown. The integrations are traits so M4/M5 can
//! replace the M3 fakes with real Soniox and Webhook adapters without changing
//! orchestration.

pub mod delivery;
pub mod retry;
pub mod routing;
pub mod transcription;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::watch;

use crate::blob::BlobStore;
use crate::domain::{Delivery, Recording, Transcript};
use crate::seam::IdGenerator;
use crate::storage::{SqliteStore, StorageError};

/// Dependencies shared by all worker ticks.
///
/// The current time is passed to each tick explicitly so retry timing is
/// deterministic in tests; [`run_worker_loop`] supplies it from a clock.
pub struct WorkerContext<'a> {
    /// Repository / work queue.
    pub store: &'a SqliteStore,
    /// Blob storage, used to resolve audio paths for Transcription.
    pub blobs: &'a BlobStore,
    /// Identifier generator for attempt and Delivery ids.
    pub ids: &'a dyn IdGenerator,
}

/// Whether a tick did a unit of work or found nothing ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkOutcome {
    /// A unit of work was processed.
    Worked,
    /// Nothing was ready to process.
    Idle,
}

/// The outcome an integration reports for a unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerFailure {
    /// A transient failure that should be retried until the deadline.
    Retryable {
        /// Machine-readable error code.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// A permanent failure that should fail the work immediately.
    Terminal {
        /// Machine-readable error code.
        code: String,
        /// Human-readable message.
        message: String,
    },
}

/// Context handed to a [`Transcriber`].
pub struct TranscriptionRequest<'a> {
    /// The Recording being transcribed.
    pub recording: &'a Recording,
    /// Absolute path to the stored audio blob.
    pub audio_path: PathBuf,
}

/// The result of a successful Transcription.
pub struct TranscriptionOutput {
    /// Speech-to-text provider name (stored on the Transcript).
    pub provider: String,
    /// Rendered plain-text Transcript.
    pub text: String,
    /// Raw provider JSON.
    pub raw_json: String,
    /// Provider-side file identifier, if any.
    pub provider_file_id: Option<String>,
    /// Provider-side transcription identifier, if any.
    pub provider_transcription_id: Option<String>,
}

/// Produces a Transcript from stored audio. Implemented by the M4 Soniox
/// adapter; a fake is used in M3 tests.
#[allow(async_fn_in_trait)]
pub trait Transcriber {
    /// Transcribe the Recording's audio.
    async fn transcribe(
        &self,
        request: TranscriptionRequest<'_>,
    ) -> Result<TranscriptionOutput, WorkerFailure>;
}

/// The routing decision for a Recording.
pub enum RoutingDecision {
    /// Select the named Sink for Delivery.
    Sink(String),
    /// No Sink matched; the Recording enters the Backlog.
    Backlog,
}

/// Selects a Sink for a Recording. Implemented by the M5 Routing Rule evaluator;
/// a fake is used in M3 tests.
pub trait Router {
    /// Decide where (if anywhere) a Recording should be delivered.
    fn route(&self, recording: &Recording) -> RoutingDecision;
}

/// Context handed to a [`SinkClient`].
pub struct DeliveryRequest<'a> {
    /// The Recording being delivered.
    pub recording: &'a Recording,
    /// The Delivery being attempted.
    pub delivery: &'a Delivery,
    /// The Transcript payload to deliver.
    pub transcript: &'a Transcript,
}

/// Delivers a Transcript payload to a Sink. Implemented by the M5 Webhook
/// adapter; a fake is used in M3 tests.
#[allow(async_fn_in_trait)]
pub trait SinkClient {
    /// Attempt the Delivery.
    async fn deliver(&self, request: DeliveryRequest<'_>) -> Result<(), WorkerFailure>;
}

/// Drive `tick` repeatedly until `shutdown` is set.
///
/// After each tick the shutdown flag is checked before any further work, so the
/// loop never starts a new tick once asked to stop. When a tick reports
/// `Worked` the next tick runs immediately; when `Idle` the loop waits
/// `idle_interval` (or until shutdown).
pub async fn run_worker_loop<F, Fut>(
    mut tick: F,
    idle_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), StorageError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<WorkOutcome, StorageError>>,
{
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }

        let outcome = tick().await?;

        if *shutdown.borrow() {
            return Ok(());
        }
        if outcome == WorkOutcome::Worked {
            continue;
        }

        tokio::select! {
            _ = tokio::time::sleep(idle_interval) => {}
            changed = shutdown.changed() => {
                if changed.is_err() {
                    // The sender was dropped; treat as shutdown.
                    return Ok(());
                }
            }
        }
    }
}
