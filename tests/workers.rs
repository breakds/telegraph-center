//! End-to-end worker tick tests with fake integrations.
//!
//! No network or real Soniox/webhook is used. Time and identifiers are injected
//! so retry timing is deterministic; most tests drive one-shot ticks.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

use tempfile::TempDir;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use tokio::sync::watch;

use telegraph_center::blob::BlobStore;
use telegraph_center::domain::{DeliveryStatus, RecordingStatus, Tags};
use telegraph_center::seam::{Clock, IdGenerator};
use telegraph_center::storage::{
    ManualRetryDelivery, ManualRetryTranscription, NewRecording, SqliteStore, StorageError,
};
use telegraph_center::workers::{
    DeliveryRequest, Router, RoutingDecision, SinkClient, Transcriber, TranscriptionOutput,
    TranscriptionRequest, WorkOutcome, WorkerContext, WorkerFailure, delivery, retry, routing,
    run_worker_loop, transcription,
};

const T0: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);

fn at(seconds: i64) -> OffsetDateTime {
    T0 + Duration::seconds(seconds)
}

// --- Injected seams --------------------------------------------------------

struct SequentialIds(AtomicU64);

impl SequentialIds {
    fn new() -> Self {
        Self(AtomicU64::new(1))
    }
}

impl IdGenerator for SequentialIds {
    fn generate(&self) -> String {
        format!("id-{}", self.0.fetch_add(1, Ordering::SeqCst))
    }
}

/// A clock that yields a scripted sequence of times, repeating the last one once
/// the script is exhausted. A tick reads it once for the attempt start time and
/// again after the integration returns for the finish time, so a two-element
/// script models an attempt that takes time to run.
struct StepClock {
    times: Vec<OffsetDateTime>,
    index: AtomicUsize,
}

impl StepClock {
    fn fixed(time: OffsetDateTime) -> Self {
        Self {
            times: vec![time],
            index: AtomicUsize::new(0),
        }
    }

    fn steps(times: Vec<OffsetDateTime>) -> Self {
        Self {
            times,
            index: AtomicUsize::new(0),
        }
    }
}

impl Clock for StepClock {
    fn now(&self) -> OffsetDateTime {
        let i = self.index.fetch_add(1, Ordering::SeqCst);
        self.times[i.min(self.times.len() - 1)]
    }
}

// --- Fake integrations -----------------------------------------------------

struct OkTranscriber {
    text: String,
    raw_json: String,
}

impl Transcriber for OkTranscriber {
    async fn transcribe(
        &self,
        _request: TranscriptionRequest<'_>,
    ) -> Result<TranscriptionOutput, WorkerFailure> {
        Ok(TranscriptionOutput {
            provider: "fake".to_string(),
            text: self.text.clone(),
            raw_json: self.raw_json.clone(),
            provider_file_id: Some("file-1".to_string()),
            provider_transcription_id: Some("tr-1".to_string()),
        })
    }
}

fn ok_transcriber() -> OkTranscriber {
    OkTranscriber {
        text: "hello world".to_string(),
        raw_json: "{}".to_string(),
    }
}

struct FailingTranscriber(WorkerFailure);

impl Transcriber for FailingTranscriber {
    async fn transcribe(
        &self,
        _request: TranscriptionRequest<'_>,
    ) -> Result<TranscriptionOutput, WorkerFailure> {
        Err(self.0.clone())
    }
}

struct BacklogRouter;

impl Router for BacklogRouter {
    fn route(&self, _recording: &telegraph_center::domain::Recording) -> RoutingDecision {
        RoutingDecision::Backlog
    }
}

struct SinkRouter(&'static str);

impl Router for SinkRouter {
    fn route(&self, _recording: &telegraph_center::domain::Recording) -> RoutingDecision {
        RoutingDecision::Sink(self.0.to_string())
    }
}

struct OkSink;

impl SinkClient for OkSink {
    async fn deliver(&self, _request: DeliveryRequest<'_>) -> Result<(), WorkerFailure> {
        Ok(())
    }
}

struct FailingSink(WorkerFailure);

impl SinkClient for FailingSink {
    async fn deliver(&self, _request: DeliveryRequest<'_>) -> Result<(), WorkerFailure> {
        Err(self.0.clone())
    }
}

fn retryable() -> WorkerFailure {
    WorkerFailure::Retryable {
        code: "503".to_string(),
        message: "service unavailable".to_string(),
    }
}

fn terminal() -> WorkerFailure {
    WorkerFailure::Terminal {
        code: "400".to_string(),
        message: "unsupported".to_string(),
    }
}

// --- Harness ---------------------------------------------------------------

async fn harness() -> (TempDir, SqliteStore, BlobStore, SequentialIds) {
    let dir = TempDir::new().unwrap();
    let store = SqliteStore::connect(dir.path().join("telegraph.db"))
        .await
        .unwrap();
    let blobs = BlobStore::new(dir.path()).await.unwrap();
    (dir, store, blobs, SequentialIds::new())
}

async fn seed_received(store: &SqliteStore, id: &str, audio_duration_ms: Option<i64>) {
    store
        .create_recording(NewRecording {
            id: id.to_string(),
            client_id: "litewatch-main".to_string(),
            client_recording_id: id.to_string(),
            original_filename: None,
            blob_path: Some(BlobStore::relative_path(id)),
            audio_size_bytes: Some(1_000),
            audio_duration_ms,
            sample_rate_hz: Some(16_000),
            channels: Some(1),
            bits_per_sample: Some(16),
            tags: Tags::default(),
            recorded_at: None,
            received_at: at(0),
        })
        .await
        .unwrap();
}

/// Drive a freshly seeded Recording to `delivering` and return its Delivery id.
async fn drive_to_delivering(
    ctx: &WorkerContext<'_>,
    id: &str,
    selected_at: OffsetDateTime,
) -> String {
    seed_received(ctx.store, id, None).await;
    transcription::tick_once(ctx, &ok_transcriber(), &StepClock::fixed(selected_at))
        .await
        .unwrap();
    routing::tick_once(ctx, &SinkRouter("journal"), &StepClock::fixed(selected_at))
        .await
        .unwrap();
    ctx.store
        .get_delivery_for_recording(id)
        .await
        .unwrap()
        .unwrap()
        .id
}

// --- Transcription + routing -----------------------------------------------

#[tokio::test]
async fn received_through_backlog_happy_path() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_received(&store, "rec-1", None).await;

    let transcriber = OkTranscriber {
        text: "hello world".to_string(),
        raw_json: r#"{"tokens":[]}"#.to_string(),
    };
    assert_eq!(
        transcription::tick_once(&ctx, &transcriber, &StepClock::fixed(at(1)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Routing);

    let transcript = store.get_transcript("rec-1").await.unwrap().unwrap();
    assert_eq!(transcript.text, "hello world");
    assert_eq!(transcript.raw_json, r#"{"tokens":[]}"#);
    assert_eq!(transcript.provider, "fake");

    assert_eq!(
        routing::tick_once(&ctx, &BacklogRouter, &StepClock::fixed(at(2)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Backlogged);
    assert!(recording.selected_sink_name.is_none());
    assert!(
        store
            .get_delivery_for_recording("rec-1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn idle_when_no_work() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    assert_eq!(
        transcription::tick_once(&ctx, &ok_transcriber(), &StepClock::fixed(at(0)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
    assert_eq!(
        routing::tick_once(&ctx, &BacklogRouter, &StepClock::fixed(at(0)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(0)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
}

#[tokio::test]
async fn received_claim_is_exclusive() {
    let (_dir, store, _blobs, _ids) = harness().await;
    seed_received(&store, "rec-1", None).await;

    let first = store.claim_received_for_transcription(at(1)).await.unwrap();
    let second = store.claim_received_for_transcription(at(1)).await.unwrap();
    assert!(first.is_some(), "first claim should win");
    assert!(
        second.is_none(),
        "second claim must not pick the same Recording"
    );
    assert_eq!(first.unwrap().status, RecordingStatus::Transcribing);
}

#[tokio::test]
async fn retryable_transcription_failure_respects_backoff() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_received(&store, "rec-1", None).await;

    // Attempt 1 fails retryably at t=1.
    assert_eq!(
        transcription::tick_once(
            &ctx,
            &FailingTranscriber(retryable()),
            &StepClock::fixed(at(1))
        )
        .await
        .unwrap(),
        WorkOutcome::Worked
    );
    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Transcribing);
    assert_eq!(
        recording.latest_error.as_deref(),
        Some("service unavailable")
    );
    let attempts = store.list_transcription_attempts("rec-1").await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].retryable);
    assert!(attempts[0].finished_at.is_some());

    // 30s later the backoff (1 minute) has not elapsed: nothing is claimed.
    assert_eq!(
        transcription::tick_once(&ctx, &ok_transcriber(), &StepClock::fixed(at(31)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
    assert_eq!(
        store
            .list_transcription_attempts("rec-1")
            .await
            .unwrap()
            .len(),
        1
    );

    // After the backoff a second attempt runs and succeeds.
    assert_eq!(
        transcription::tick_once(&ctx, &ok_transcriber(), &StepClock::fixed(at(62)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    let attempts = store.list_transcription_attempts("rec-1").await.unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].attempt_number, 2);
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Routing
    );
}

#[tokio::test]
async fn transcription_backoff_is_measured_from_failure_not_start() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_received(&store, "rec-1", None).await;

    // Attempt 1 starts at t=0 and the call takes 10 minutes before failing.
    assert_eq!(
        transcription::tick_once(
            &ctx,
            &FailingTranscriber(retryable()),
            &StepClock::steps(vec![at(0), at(600)]),
        )
        .await
        .unwrap(),
        WorkOutcome::Worked
    );
    let attempts = store.list_transcription_attempts("rec-1").await.unwrap();
    assert_eq!(attempts[0].started_at, at(0));
    assert_eq!(attempts[0].finished_at, Some(at(600)));

    // If backoff were measured from the start (t=0 + 1m), this would retry.
    // Measured from the failure (t=10m + 1m = t=11m), it must not yet.
    assert_eq!(
        transcription::tick_once(&ctx, &ok_transcriber(), &StepClock::fixed(at(630)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
    assert_eq!(
        store
            .list_transcription_attempts("rec-1")
            .await
            .unwrap()
            .len(),
        1
    );

    // At t=11m the backoff from the failure has elapsed.
    assert_eq!(
        transcription::tick_once(&ctx, &ok_transcriber(), &StepClock::fixed(at(660)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    assert_eq!(
        store
            .list_transcription_attempts("rec-1")
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn terminal_transcription_failure_moves_to_failed() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_received(&store, "rec-1", None).await;

    transcription::tick_once(
        &ctx,
        &FailingTranscriber(terminal()),
        &StepClock::fixed(at(1)),
    )
    .await
    .unwrap();
    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::TranscriptionFailed);
    assert_eq!(recording.latest_error.as_deref(), Some("unsupported"));
    let attempts = store.list_transcription_attempts("rec-1").await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert!(!attempts[0].retryable);
}

#[tokio::test]
async fn retryable_transcription_failure_after_deadline_becomes_terminal() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    // No audio duration -> 2-hour retry window anchored on the first attempt.
    seed_received(&store, "rec-1", None).await;

    // Attempt 1 at t=0 fails retryably; first attempt anchors deadline at +2h.
    transcription::tick_once(
        &ctx,
        &FailingTranscriber(retryable()),
        &StepClock::fixed(at(0)),
    )
    .await
    .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Transcribing
    );

    // Attempt 2 three hours later is past the deadline: retryable becomes terminal.
    let three_hours = 3 * 3600;
    transcription::tick_once(
        &ctx,
        &FailingTranscriber(retryable()),
        &StepClock::fixed(at(three_hours)),
    )
    .await
    .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::TranscriptionFailed
    );
}

// --- Routing to a Sink + delivery ------------------------------------------

#[tokio::test]
async fn routing_to_sink_creates_single_delivery() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(10)).await;

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Delivering);
    assert_eq!(recording.selected_sink_name.as_deref(), Some("journal"));

    let delivery = store.get_delivery(&delivery_id).await.unwrap().unwrap();
    assert_eq!(delivery.status, DeliveryStatus::Delivering);
    assert_eq!(delivery.sink_name, "journal");
    // Delivery retry deadline is 24 hours from Sink selection.
    assert_eq!(
        delivery.retry_deadline_at,
        Some(at(10) + Duration::hours(24))
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deliveries WHERE recording_id = ?")
        .bind("rec-1")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn successful_delivery_moves_to_delivered() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(10)).await;

    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(11)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivered
    );
    assert_eq!(
        store
            .get_delivery(&delivery_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DeliveryStatus::Delivered
    );
}

#[tokio::test]
async fn retryable_delivery_failure_respects_backoff() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(0)).await;

    // First delivery attempt fails retryably.
    assert_eq!(
        delivery::tick_once(&ctx, &FailingSink(retryable()), &StepClock::fixed(at(1)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    assert_eq!(
        store
            .get_delivery(&delivery_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DeliveryStatus::Delivering
    );
    assert_eq!(
        store
            .list_delivery_attempts(&delivery_id)
            .await
            .unwrap()
            .len(),
        1
    );

    // Before the 1-minute backoff elapses nothing is retried.
    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(31)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
    assert_eq!(
        store
            .list_delivery_attempts(&delivery_id)
            .await
            .unwrap()
            .len(),
        1
    );

    // After the backoff a second attempt succeeds.
    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(62)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    assert_eq!(
        store
            .list_delivery_attempts(&delivery_id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivered
    );
}

#[tokio::test]
async fn delivery_backoff_is_measured_from_failure_not_start() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(0)).await;

    // First attempt starts at t=0 and the call takes 10 minutes before failing.
    assert_eq!(
        delivery::tick_once(
            &ctx,
            &FailingSink(retryable()),
            &StepClock::steps(vec![at(0), at(600)]),
        )
        .await
        .unwrap(),
        WorkOutcome::Worked
    );
    let attempts = store.list_delivery_attempts(&delivery_id).await.unwrap();
    assert_eq!(attempts[0].started_at, at(0));
    assert_eq!(attempts[0].finished_at, Some(at(600)));

    // Measured from the failure (t=10m + 1m), not the start, so t=10m30s is early.
    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(630)))
            .await
            .unwrap(),
        WorkOutcome::Idle
    );
    assert_eq!(
        store
            .list_delivery_attempts(&delivery_id)
            .await
            .unwrap()
            .len(),
        1
    );

    // At t=11m the backoff from the failure has elapsed.
    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(660)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivered
    );
}

#[tokio::test]
async fn terminal_delivery_failure_moves_to_delivery_failed() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(0)).await;

    delivery::tick_once(&ctx, &FailingSink(terminal()), &StepClock::fixed(at(1)))
        .await
        .unwrap();

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::DeliveryFailed);
    assert_ne!(recording.status, RecordingStatus::Backlogged);
    assert_eq!(
        store
            .get_delivery(&delivery_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DeliveryStatus::DeliveryFailed
    );
}

#[tokio::test]
async fn delivery_retry_after_deadline_becomes_failed() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    // Selected at t=0, so the 24h deadline is at t=24h.
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(0)).await;

    // A retryable failure 25 hours later is past the deadline: terminal.
    let twenty_five_hours = 25 * 3600;
    delivery::tick_once(
        &ctx,
        &FailingSink(retryable()),
        &StepClock::fixed(at(twenty_five_hours)),
    )
    .await
    .unwrap();

    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::DeliveryFailed
    );
    assert_eq!(
        store
            .get_delivery(&delivery_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        DeliveryStatus::DeliveryFailed
    );
}

// --- Worker loop -----------------------------------------------------------

#[tokio::test]
async fn worker_loop_does_not_tick_when_already_shut_down() {
    let (tx, rx) = watch::channel(true);
    let _ = tx; // keep sender alive
    let counter = Arc::new(AtomicU64::new(0));
    let counter_in = counter.clone();

    run_worker_loop(
        move || {
            let counter = counter_in.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok::<_, StorageError>(WorkOutcome::Idle)
            }
        },
        StdDuration::from_millis(10),
        rx,
    )
    .await
    .unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn worker_loop_stops_after_shutdown_without_another_tick() {
    let (tx, rx) = watch::channel(false);
    let tx = Arc::new(tx);
    let counter = Arc::new(AtomicU64::new(0));
    let counter_in = counter.clone();
    let tx_in = tx.clone();

    run_worker_loop(
        move || {
            let counter = counter_in.clone();
            let tx = tx_in.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // Ask for shutdown during the first tick.
                tx.send(true).unwrap();
                Ok::<_, StorageError>(WorkOutcome::Worked)
            }
        },
        StdDuration::from_millis(10),
        rx,
    )
    .await
    .unwrap();

    // Exactly one tick ran; the loop stopped before starting another.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

// --- Retry policy sanity (covered in depth by unit tests in retry.rs) ------

#[test]
fn delivery_window_is_24_hours() {
    assert_eq!(retry::DELIVERY_WINDOW, Duration::hours(24));
}

// --- Manual retry makes work immediately due (M7) --------------------------

#[tokio::test]
async fn manual_retry_transcription_is_due_within_backoff_window() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    seed_received(&store, "rec-1", None).await;

    // A failed attempt at t=0 leaves the Recording transcription_failed.
    transcription::tick_once(
        &ctx,
        &FailingTranscriber(terminal()),
        &StepClock::fixed(at(0)),
    )
    .await
    .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::TranscriptionFailed
    );

    // Operator retries at t=10s.
    store
        .manual_retry_transcription(ManualRetryTranscription {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            at: at(10),
            actor_id: Some("break".to_string()),
        })
        .await
        .unwrap();

    // At t=20s — well within the 60s backoff from the t=0 failure — the worker
    // still picks it up immediately because of the manual retry, and succeeds.
    assert!(at(20) < retry::next_retry_at(at(0), 1));
    assert_eq!(
        transcription::tick_once(&ctx, &ok_transcriber(), &StepClock::fixed(at(20)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Routing
    );
}

#[tokio::test]
async fn manual_retry_transcription_resets_the_deadline_window() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    // No audio duration -> 2-hour retry window.
    seed_received(&store, "rec-1", None).await;
    transcription::tick_once(
        &ctx,
        &FailingTranscriber(terminal()),
        &StepClock::fixed(at(0)),
    )
    .await
    .unwrap();

    // Retry long after the original 2h window (at t=0) would have closed.
    let long_after = 10_000;
    store
        .manual_retry_transcription(ManualRetryTranscription {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            at: at(long_after),
            actor_id: None,
        })
        .await
        .unwrap();

    // A retryable failure within the *fresh* window stays transcribing rather
    // than going terminal — proving the deadline re-anchored on the retry.
    transcription::tick_once(
        &ctx,
        &FailingTranscriber(retryable()),
        &StepClock::steps(vec![at(long_after), at(long_after + 1)]),
    )
    .await
    .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Transcribing
    );
}

#[tokio::test]
async fn manual_retry_delivery_is_due_within_backoff_window() {
    let (_dir, store, blobs, ids) = harness().await;
    let ctx = WorkerContext {
        store: &store,
        blobs: &blobs,
        ids: &ids,
    };
    let delivery_id = drive_to_delivering(&ctx, "rec-1", at(0)).await;

    // A failed delivery attempt at t=0 leaves the Recording delivery_failed.
    delivery::tick_once(&ctx, &FailingSink(terminal()), &StepClock::fixed(at(0)))
        .await
        .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::DeliveryFailed
    );

    store
        .manual_retry_delivery(ManualRetryDelivery {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            at: at(10),
            retry_deadline_at: at(10 + 86_400),
            actor_id: Some("break".to_string()),
        })
        .await
        .unwrap();

    // At t=20s — within the 60s backoff from the t=0 failure — the worker picks
    // it up immediately because of the manual retry, and delivers.
    assert!(at(20) < retry::next_retry_at(at(0), 1));
    assert_eq!(
        delivery::tick_once(&ctx, &OkSink, &StepClock::fixed(at(20)))
            .await
            .unwrap(),
        WorkOutcome::Worked
    );
    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Delivered);
    let _ = delivery_id;
}
