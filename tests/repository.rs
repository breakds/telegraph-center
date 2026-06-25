//! Repository tests for the SQLite store.
//!
//! Each test uses a fresh temporary database, runs migrations on connect, and
//! injects identifiers and timestamps so behavior is deterministic and free of
//! network or external-service dependencies.

use tempfile::TempDir;
use time::OffsetDateTime;
use time::macros::datetime;

use telegraph_center::domain::{DeliveryStatus, RecordingStatus, Tags};
use telegraph_center::storage::{
    ManualRetryDelivery, ManualRetryTranscription, ManualRoute, MonitorStatusFilter, NewAuditEvent,
    NewDelivery, NewDeliveryAttempt, NewLoginFailure, NewOperatorSession, NewRecording,
    NewTranscript, NewTranscriptionAttempt, RoutingOutcome, SqliteStore, StorageError,
};

const T0: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);

fn at(seconds: i64) -> OffsetDateTime {
    T0 + time::Duration::seconds(seconds)
}

async fn fresh_store() -> (TempDir, SqliteStore) {
    let dir = TempDir::new().expect("temp dir");
    let store = SqliteStore::connect(dir.path().join("telegraph.db"))
        .await
        .expect("connect and migrate");
    (dir, store)
}

fn new_recording(id: &str, client: &str, client_recording_id: &str) -> NewRecording {
    NewRecording {
        id: id.to_string(),
        client_id: client.to_string(),
        client_recording_id: client_recording_id.to_string(),
        original_filename: Some("20260623-120000.wav".to_string()),
        blob_path: Some(format!("recordings/{id}.wav")),
        audio_size_bytes: Some(1_024),
        audio_duration_ms: Some(5_000),
        sample_rate_hz: Some(16_000),
        channels: Some(1),
        bits_per_sample: Some(16),
        tags: Tags::new(["journal"]).unwrap(),
        recorded_at: Some(at(-30)),
        received_at: at(0),
    }
}

/// Drive a Recording up to `delivering` with a selected Sink, returning its id
/// and the Delivery id.
async fn deliver_setup(store: &SqliteStore, recording_id: &str) -> String {
    store
        .create_recording(new_recording(recording_id, "litewatch-main", recording_id))
        .await
        .unwrap();
    store
        .update_recording_status(recording_id, RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .store_transcript(NewTranscript {
            recording_id: recording_id.to_string(),
            provider: "soniox".to_string(),
            text: "hello world".to_string(),
            raw_json: "{}".to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: at(2),
        })
        .await
        .unwrap();
    let delivery_id = format!("{recording_id}-delivery");
    store
        .select_sink(NewDelivery {
            id: delivery_id.clone(),
            recording_id: recording_id.to_string(),
            sink_name: "journal".to_string(),
            selected_at: at(3),
            retry_deadline_at: Some(at(3 + 86_400)),
        })
        .await
        .unwrap();
    delivery_id
}

#[tokio::test]
async fn migrations_run_on_fresh_database() {
    let (_dir, store) = fresh_store().await;
    // A query against a migrated table proves the schema exists.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recordings")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn recording_insert_succeeds() {
    let (_dir, store) = fresh_store().await;
    let creation = store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();

    assert!(creation.created);
    assert_eq!(creation.recording.status, RecordingStatus::Received);
    assert_eq!(creation.recording.client_id, "litewatch-main");
    assert_eq!(creation.recording.tags.as_slice(), &["journal".to_string()]);
    assert_eq!(creation.recording.received_at, at(0));
    assert_eq!(creation.recording.recorded_at, Some(at(-30)));

    let fetched = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(fetched, creation.recording);
}

#[tokio::test]
async fn duplicate_idempotency_key_returns_existing_recording() {
    let (_dir, store) = fresh_store().await;
    let first = store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    assert!(first.created);

    // Same idempotency key, different server id: must not create a second row.
    let second = store
        .create_recording(new_recording("rec-2", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    assert!(!second.created);
    assert_eq!(second.recording.id, "rec-1");

    let by_key = store
        .get_recording_by_idempotency("litewatch-main", "cr-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_key.id, "rec-1");

    // The same client_recording_id under a different Client is independent.
    let other_client = store
        .create_recording(new_recording("rec-3", "other-client", "cr-1"))
        .await
        .unwrap();
    assert!(other_client.created);
    assert_eq!(other_client.recording.id, "rec-3");
}

#[tokio::test]
async fn happy_path_status_transitions() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();

    let r = store
        .update_recording_status("rec-1", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    assert_eq!(r.status, RecordingStatus::Transcribing);
    assert_eq!(r.updated_at, at(1));

    // transcribing -> routing happens via store_transcript.
    store
        .store_transcript(NewTranscript {
            recording_id: "rec-1".to_string(),
            provider: "soniox".to_string(),
            text: "hi".to_string(),
            raw_json: "{}".to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: at(2),
        })
        .await
        .unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Routing
    );

    // routing -> delivering via select_sink.
    store
        .select_sink(NewDelivery {
            id: "del-1".to_string(),
            recording_id: "rec-1".to_string(),
            sink_name: "journal".to_string(),
            selected_at: at(3),
            retry_deadline_at: None,
        })
        .await
        .unwrap();
    let rec = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(rec.status, RecordingStatus::Delivering);
    assert_eq!(rec.selected_sink_name.as_deref(), Some("journal"));

    // delivering -> delivered.
    store.mark_delivery_delivered("del-1", at(4)).await.unwrap();
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivered
    );
}

#[tokio::test]
async fn transcription_failure_transition() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    store
        .update_recording_status("rec-1", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    let r = store
        .update_recording_status("rec-1", RecordingStatus::TranscriptionFailed, at(2))
        .await
        .unwrap();
    assert_eq!(r.status, RecordingStatus::TranscriptionFailed);
}

#[tokio::test]
async fn invalid_transition_is_rejected() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    // received -> delivered is not a legal transition.
    let err = store
        .update_recording_status("rec-1", RecordingStatus::Delivered, at(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::InvalidTransition(_)));
    // The status is unchanged.
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Received
    );
}

#[tokio::test]
async fn backlog_is_distinct_from_delivery_failure() {
    let (_dir, store) = fresh_store().await;

    // Backlogged recording: routed but no Sink selected.
    store
        .create_recording(new_recording("rec-backlog", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    store
        .update_recording_status("rec-backlog", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .store_transcript(NewTranscript {
            recording_id: "rec-backlog".to_string(),
            provider: "soniox".to_string(),
            text: "no match".to_string(),
            raw_json: "{}".to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: at(2),
        })
        .await
        .unwrap();
    let backlogged = store.mark_backlogged("rec-backlog", at(3)).await.unwrap();
    assert_eq!(backlogged.status, RecordingStatus::Backlogged);
    assert!(backlogged.selected_sink_name.is_none());
    assert!(
        store
            .get_delivery_for_recording("rec-backlog")
            .await
            .unwrap()
            .is_none()
    );

    // Delivery failure: a Sink was selected, then delivery failed.
    let delivery_id = deliver_setup(&store, "rec-fail").await;
    let delivery = store
        .mark_delivery_failed(&delivery_id, at(10), "connection refused")
        .await
        .unwrap();
    assert_eq!(delivery.status, DeliveryStatus::DeliveryFailed);
    let rec = store.get_recording("rec-fail").await.unwrap().unwrap();
    assert_eq!(rec.status, RecordingStatus::DeliveryFailed);
    assert_eq!(rec.selected_sink_name.as_deref(), Some("journal"));
    assert_eq!(rec.latest_error.as_deref(), Some("connection refused"));

    // A delivery-failed recording is never in the Backlog state.
    assert_ne!(rec.status, RecordingStatus::Backlogged);
}

#[tokio::test]
async fn transcript_preserves_text_and_raw_json() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    store
        .update_recording_status("rec-1", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();

    let raw = r#"{"tokens":[{"text":"hello","speaker":"1"}]}"#;
    store
        .store_transcript(NewTranscript {
            recording_id: "rec-1".to_string(),
            provider: "soniox".to_string(),
            text: "hello world".to_string(),
            raw_json: raw.to_string(),
            provider_file_id: Some("file-123".to_string()),
            provider_transcription_id: Some("tr-456".to_string()),
            created_at: at(2),
        })
        .await
        .unwrap();

    let transcript = store.get_transcript("rec-1").await.unwrap().unwrap();
    assert_eq!(transcript.text, "hello world");
    assert_eq!(transcript.raw_json, raw);
    assert_eq!(transcript.provider_file_id.as_deref(), Some("file-123"));
    assert_eq!(
        transcript.provider_transcription_id.as_deref(),
        Some("tr-456")
    );
}

#[tokio::test]
async fn delivery_has_stable_id_and_is_unique_per_recording() {
    let (_dir, store) = fresh_store().await;
    let delivery_id = deliver_setup(&store, "rec-1").await;

    let delivery = store.get_delivery(&delivery_id).await.unwrap().unwrap();
    assert_eq!(delivery.id, delivery_id);
    assert_eq!(delivery.status, DeliveryStatus::Delivering);
    assert_eq!(delivery.recording_id, "rec-1");

    // A second Sink selection for the same Recording must be rejected.
    let err = store
        .select_sink(NewDelivery {
            id: "another-delivery".to_string(),
            recording_id: "rec-1".to_string(),
            sink_name: "todo".to_string(),
            selected_at: at(5),
            retry_deadline_at: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Database(_)));

    // The original Delivery is unchanged.
    let still = store
        .get_delivery_for_recording("rec-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.id, delivery_id);
}

#[tokio::test]
async fn transcription_and_delivery_attempts_are_stored_and_queryable() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    store
        .update_recording_status("rec-1", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();

    store
        .insert_transcription_attempt(NewTranscriptionAttempt {
            id: "ta-1".to_string(),
            recording_id: "rec-1".to_string(),
            attempt_number: 1,
            started_at: at(1),
            finished_at: Some(at(2)),
            status: "failed".to_string(),
            retryable: true,
            error_code: Some("429".to_string()),
            error_message: Some("rate limited".to_string()),
        })
        .await
        .unwrap();

    let attempts = store.list_transcription_attempts("rec-1").await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].id, "ta-1");
    assert!(attempts[0].retryable);
    assert_eq!(attempts[0].error_code.as_deref(), Some("429"));

    let delivery_id = deliver_setup(&store, "rec-2").await;
    store
        .insert_delivery_attempt(NewDeliveryAttempt {
            id: "da-1".to_string(),
            delivery_id: delivery_id.clone(),
            attempt_number: 1,
            started_at: at(3),
            finished_at: Some(at(4)),
            status: "failed".to_string(),
            http_status: Some(503),
            retryable: true,
            error_message: Some("service unavailable".to_string()),
        })
        .await
        .unwrap();

    let delivery_attempts = store.list_delivery_attempts(&delivery_id).await.unwrap();
    assert_eq!(delivery_attempts.len(), 1);
    assert_eq!(delivery_attempts[0].http_status, Some(503));
    assert!(delivery_attempts[0].retryable);
}

#[tokio::test]
async fn operator_session_can_be_created_fetched_and_revoked() {
    let (_dir, store) = fresh_store().await;
    let created = store
        .create_session(NewOperatorSession {
            session_hash: "sess-hash".to_string(),
            operator_username: "break".to_string(),
            csrf_token_hash: "csrf-hash".to_string(),
            created_at: at(0),
            last_seen_at: at(0),
            idle_expires_at: at(7 * 86_400),
            absolute_expires_at: at(14 * 86_400),
        })
        .await
        .unwrap();
    assert!(created.revoked_at.is_none());

    let fetched = store.get_session("sess-hash").await.unwrap().unwrap();
    assert_eq!(fetched, created);

    assert!(store.revoke_session("sess-hash", at(100)).await.unwrap());
    let revoked = store.get_session("sess-hash").await.unwrap().unwrap();
    assert_eq!(revoked.revoked_at, Some(at(100)));

    // Revoking again is a no-op.
    assert!(!store.revoke_session("sess-hash", at(200)).await.unwrap());
}

#[tokio::test]
async fn touch_session_refreshes_idle_window_but_not_revoked_sessions() {
    let (_dir, store) = fresh_store().await;
    store
        .create_session(NewOperatorSession {
            session_hash: "sess-hash".to_string(),
            operator_username: "break".to_string(),
            csrf_token_hash: "csrf-hash".to_string(),
            created_at: at(0),
            last_seen_at: at(0),
            idle_expires_at: at(7 * 86_400),
            absolute_expires_at: at(14 * 86_400),
        })
        .await
        .unwrap();

    let touched = store
        .touch_session("sess-hash", at(3_600), at(3_600 + 7 * 86_400))
        .await
        .unwrap();
    assert!(touched);

    let refreshed = store.get_session("sess-hash").await.unwrap().unwrap();
    assert_eq!(refreshed.last_seen_at, at(3_600));
    assert_eq!(refreshed.idle_expires_at, at(3_600 + 7 * 86_400));

    // A revoked session cannot be touched back to life.
    assert!(store.revoke_session("sess-hash", at(7_200)).await.unwrap());
    let touched_revoked = store
        .touch_session("sess-hash", at(10_000), at(10_000 + 7 * 86_400))
        .await
        .unwrap();
    assert!(!touched_revoked);
}

#[tokio::test]
async fn clear_login_failures_removes_matching_rows() {
    let (_dir, store) = fresh_store().await;
    for (i, (username, ip)) in [
        ("break", "10.0.0.1"),
        ("break", "10.0.0.2"),
        ("someone", "10.0.0.1"),
        ("someone", "192.168.1.1"),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .record_login_failure(NewLoginFailure {
                id: format!("lf-{i}"),
                username: username.to_string(),
                remote_ip: ip.to_string(),
                failed_at: at(0),
            })
            .await
            .unwrap();
    }

    // Clears rows matching the username OR the IP (the first three rows).
    let removed = store
        .clear_login_failures("break", "10.0.0.1")
        .await
        .unwrap();
    assert_eq!(removed, 3);

    // The unrelated username/IP row survives.
    let remaining = store
        .count_login_failures_since("someone", "192.168.1.1", at(0))
        .await
        .unwrap();
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn login_failures_are_counted_within_window() {
    let (_dir, store) = fresh_store().await;
    for (i, seconds) in [0, 30, 60].into_iter().enumerate() {
        store
            .record_login_failure(NewLoginFailure {
                id: format!("lf-{i}"),
                username: "break".to_string(),
                remote_ip: "10.0.0.1".to_string(),
                failed_at: at(seconds),
            })
            .await
            .unwrap();
    }

    let count = store
        .count_login_failures_since("break", "10.0.0.1", at(0))
        .await
        .unwrap();
    assert_eq!(count, 3);

    // Counting from a later cutoff excludes earlier failures.
    let recent = store
        .count_login_failures_since("break", "10.0.0.1", at(45))
        .await
        .unwrap();
    assert_eq!(recent, 1);

    // An unrelated username/IP pair matches nothing.
    let none = store
        .count_login_failures_since("someone", "192.168.1.1", at(0))
        .await
        .unwrap();
    assert_eq!(none, 0);
}

#[tokio::test]
async fn audit_events_are_appended_and_queryable() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();

    store
        .insert_audit_event(NewAuditEvent {
            id: "ae-1".to_string(),
            occurred_at: at(0),
            actor_kind: "operator".to_string(),
            actor_id: Some("break".to_string()),
            event_type: "manual_routing".to_string(),
            recording_id: Some("rec-1".to_string()),
            details_json: r#"{"sink":"journal"}"#.to_string(),
        })
        .await
        .unwrap();

    let events = store
        .list_audit_events_for_recording("rec-1")
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "manual_routing");
    assert_eq!(events[0].details_json, r#"{"sink":"journal"}"#);
}

/// Drive a Recording to `routing` (transcribed, awaiting a Sink decision).
async fn route_setup(store: &SqliteStore, recording_id: &str) {
    store
        .create_recording(new_recording(recording_id, "litewatch-main", recording_id))
        .await
        .unwrap();
    store
        .update_recording_status(recording_id, RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .store_transcript(NewTranscript {
            recording_id: recording_id.to_string(),
            provider: "soniox".to_string(),
            text: "hello world".to_string(),
            raw_json: "{}".to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: at(2),
        })
        .await
        .unwrap();
}

fn delivery_for(recording_id: &str, delivery_id: &str, at_secs: i64) -> NewDelivery {
    NewDelivery {
        id: delivery_id.to_string(),
        recording_id: recording_id.to_string(),
        sink_name: "journal".to_string(),
        selected_at: at(at_secs),
        retry_deadline_at: Some(at(at_secs + 86_400)),
    }
}

#[tokio::test]
async fn route_to_sink_selects_and_is_idempotent() {
    let (_dir, store) = fresh_store().await;
    route_setup(&store, "rec-1").await;

    let outcome = store
        .route_to_sink(delivery_for("rec-1", "del-1", 3))
        .await
        .unwrap();
    let RoutingOutcome::Selected(delivery) = outcome else {
        panic!("expected a Sink to be selected");
    };
    assert_eq!(delivery.id, "del-1");
    assert_eq!(delivery.sink_name, "journal");
    assert_eq!(
        store.get_recording("rec-1").await.unwrap().unwrap().status,
        RecordingStatus::Delivering
    );

    // A second routing attempt (the Recording is no longer `routing`) is a
    // benign no-op and must not create a second Delivery.
    let again = store
        .route_to_sink(delivery_for("rec-1", "del-2", 4))
        .await
        .unwrap();
    assert_eq!(again, RoutingOutcome::AlreadyHandled);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deliveries WHERE recording_id = ?")
        .bind("rec-1")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn manual_route_from_backlog_creates_delivery_and_audit() {
    let (_dir, store) = fresh_store().await;
    route_setup(&store, "rec-1").await;
    store.mark_backlogged("rec-1", at(3)).await.unwrap();

    let delivery = store
        .manual_route(ManualRoute {
            recording_id: "rec-1".to_string(),
            sink_name: "journal".to_string(),
            delivery_id: "del-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            selected_at: at(100),
            retry_deadline_at: at(100 + 86_400),
            actor_id: Some("break".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(delivery.id, "del-1");
    assert_eq!(delivery.sink_name, "journal");
    assert_eq!(delivery.status, DeliveryStatus::Delivering);
    assert_eq!(delivery.retry_deadline_at, Some(at(100 + 86_400)));

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Delivering);
    assert_eq!(recording.selected_sink_name.as_deref(), Some("journal"));

    let events = store
        .list_audit_events_for_recording("rec-1")
        .await
        .unwrap();
    let manual = events
        .iter()
        .find(|event| event.event_type == "manual_routing")
        .expect("manual_routing audit event");
    assert_eq!(manual.details_json, r#"{"sink":"journal"}"#);
    assert_eq!(manual.actor_id.as_deref(), Some("break"));
}

#[tokio::test]
async fn manual_route_rejects_non_backlogged_recording() {
    let (_dir, store) = fresh_store().await;
    route_setup(&store, "rec-1").await; // status is `routing`, not `backlogged`

    let err = store
        .manual_route(ManualRoute {
            recording_id: "rec-1".to_string(),
            sink_name: "journal".to_string(),
            delivery_id: "del-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            selected_at: at(100),
            retry_deadline_at: at(100 + 86_400),
            actor_id: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::RecordingNotBacklogged { .. }));

    // No Delivery and no audit event were created.
    assert!(
        store
            .get_delivery_for_recording("rec-1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn manual_route_rejects_unknown_recording() {
    let (_dir, store) = fresh_store().await;
    let err = store
        .manual_route(ManualRoute {
            recording_id: "missing".to_string(),
            sink_name: "journal".to_string(),
            delivery_id: "del-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            selected_at: at(100),
            retry_deadline_at: at(100 + 86_400),
            actor_id: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::RecordingNotFound(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_route_to_sink_selects_exactly_once() {
    let (_dir, store) = fresh_store().await;
    route_setup(&store, "rec-1").await;

    // Two workers race to route the same `routing` Recording.
    let s1 = store.clone();
    let s2 = store.clone();
    let (a, b) = tokio::join!(
        s1.route_to_sink(delivery_for("rec-1", "del-a", 3)),
        s2.route_to_sink(delivery_for("rec-1", "del-b", 3)),
    );
    let outcomes = [a.unwrap(), b.unwrap()];

    let selected = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RoutingOutcome::Selected(_)))
        .count();
    let already = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RoutingOutcome::AlreadyHandled))
        .count();
    assert_eq!(selected, 1, "exactly one worker should win");
    assert_eq!(
        already, 1,
        "the other should see AlreadyHandled, not an error"
    );

    // Exactly one Delivery exists for the Recording.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deliveries WHERE recording_id = ?")
        .bind("rec-1")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// ---------------------------------------------------------------------------
// M7 monitor read models and manual retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_summaries_filters_by_status() {
    let (_dir, store) = fresh_store().await;

    // received
    store
        .create_recording(new_recording("r-recv", "litewatch-main", "r-recv"))
        .await
        .unwrap();

    // backlogged
    store
        .create_recording(new_recording("r-back", "litewatch-main", "r-back"))
        .await
        .unwrap();
    store
        .update_recording_status("r-back", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .store_transcript(NewTranscript {
            recording_id: "r-back".to_string(),
            provider: "soniox".to_string(),
            text: "hi".to_string(),
            raw_json: "{}".to_string(),
            provider_file_id: None,
            provider_transcription_id: None,
            created_at: at(2),
        })
        .await
        .unwrap();
    store.mark_backlogged("r-back", at(3)).await.unwrap();

    // transcription_failed
    store
        .create_recording(new_recording("r-tf", "litewatch-main", "r-tf"))
        .await
        .unwrap();
    store
        .update_recording_status("r-tf", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .update_recording_status("r-tf", RecordingStatus::TranscriptionFailed, at(2))
        .await
        .unwrap();

    // delivering
    deliver_setup(&store, "r-delivering").await;

    // delivered
    let delivered_id = deliver_setup(&store, "r-delivered").await;
    store
        .mark_delivery_delivered(&delivered_id, at(10))
        .await
        .unwrap();

    // delivery_failed
    let failed_id = deliver_setup(&store, "r-df").await;
    store
        .mark_delivery_failed(&failed_id, at(10), "boom")
        .await
        .unwrap();

    async fn ids(store: &SqliteStore, filter: MonitorStatusFilter) -> Vec<String> {
        let mut ids: Vec<String> = store
            .list_recording_summaries(filter, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        ids.sort();
        ids
    }

    assert_eq!(ids(&store, MonitorStatusFilter::All).await.len(), 6);
    assert_eq!(
        ids(&store, MonitorStatusFilter::Backlogged).await,
        vec!["r-back"]
    );
    assert_eq!(
        ids(&store, MonitorStatusFilter::Failed).await,
        vec!["r-df", "r-tf"]
    );
    assert_eq!(
        ids(&store, MonitorStatusFilter::Delivering).await,
        vec!["r-delivering"]
    );
    assert_eq!(
        ids(&store, MonitorStatusFilter::Delivered).await,
        vec!["r-delivered"]
    );
}

#[tokio::test]
async fn list_summaries_are_newest_first_and_respect_limit() {
    let (_dir, store) = fresh_store().await;
    for (i, seconds) in [0_i64, 100, 200].into_iter().enumerate() {
        let mut new = new_recording(&format!("r-{i}"), "litewatch-main", &format!("cr-{i}"));
        new.received_at = at(seconds);
        store.create_recording(new).await.unwrap();
    }

    let summaries = store
        .list_recording_summaries(MonitorStatusFilter::All, 2)
        .await
        .unwrap();
    // Limit honored, newest received_at first.
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, "r-2");
    assert_eq!(summaries[1].id, "r-1");
}

#[tokio::test]
async fn recording_detail_composes_transcript_delivery_attempts_and_audit() {
    let (_dir, store) = fresh_store().await;
    let delivery_id = deliver_setup(&store, "rec-1").await;

    store
        .insert_transcription_attempt(NewTranscriptionAttempt {
            id: "ta-1".to_string(),
            recording_id: "rec-1".to_string(),
            attempt_number: 1,
            started_at: at(1),
            finished_at: Some(at(2)),
            status: "succeeded".to_string(),
            retryable: false,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    store
        .insert_delivery_attempt(NewDeliveryAttempt {
            id: "da-1".to_string(),
            delivery_id: delivery_id.clone(),
            attempt_number: 1,
            started_at: at(3),
            finished_at: Some(at(4)),
            status: "failed".to_string(),
            http_status: Some(503),
            retryable: true,
            error_message: Some("upstream".to_string()),
        })
        .await
        .unwrap();
    store
        .insert_audit_event(NewAuditEvent {
            id: "ae-1".to_string(),
            occurred_at: at(5),
            actor_kind: "operator".to_string(),
            actor_id: Some("break".to_string()),
            event_type: "manual_routing".to_string(),
            recording_id: Some("rec-1".to_string()),
            details_json: r#"{"sink":"journal"}"#.to_string(),
        })
        .await
        .unwrap();

    let detail = store.get_recording_detail("rec-1").await.unwrap().unwrap();
    assert_eq!(detail.recording.id, "rec-1");
    assert_eq!(detail.transcript.unwrap().text, "hello world");
    assert_eq!(detail.delivery.unwrap().id, delivery_id);
    assert_eq!(detail.transcription_attempts.len(), 1);
    assert_eq!(detail.delivery_attempts.len(), 1);
    assert_eq!(detail.audit_events.len(), 1);

    assert!(store.get_recording_detail("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn manual_retry_transcription_only_from_failed() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();

    // Wrong state (received) is rejected and changes nothing.
    let err = store
        .manual_retry_transcription(ManualRetryTranscription {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            at: at(5),
            actor_id: Some("break".to_string()),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::RecordingNotRetryable { kind, .. } if kind == "transcription"
    ));

    store
        .update_recording_status("rec-1", RecordingStatus::Transcribing, at(1))
        .await
        .unwrap();
    store
        .update_recording_status("rec-1", RecordingStatus::TranscriptionFailed, at(2))
        .await
        .unwrap();

    let recording = store
        .manual_retry_transcription(ManualRetryTranscription {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-2".to_string(),
            at: at(10),
            actor_id: Some("break".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(recording.status, RecordingStatus::Transcribing);
    assert!(recording.latest_error.is_none());

    let events = store
        .list_audit_events_for_recording("rec-1")
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "manual_retry_transcription")
    );
}

#[tokio::test]
async fn manual_retry_delivery_only_from_failed_and_preserves_sink() {
    let (_dir, store) = fresh_store().await;
    let delivery_id = deliver_setup(&store, "rec-1").await;

    // Wrong state (delivering) is rejected.
    let err = store
        .manual_retry_delivery(ManualRetryDelivery {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-1".to_string(),
            at: at(5),
            retry_deadline_at: at(5 + 86_400),
            actor_id: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::RecordingNotRetryable { kind, .. } if kind == "delivery"
    ));

    store
        .mark_delivery_failed(&delivery_id, at(6), "boom")
        .await
        .unwrap();

    let delivery = store
        .manual_retry_delivery(ManualRetryDelivery {
            recording_id: "rec-1".to_string(),
            audit_event_id: "ae-2".to_string(),
            at: at(10),
            retry_deadline_at: at(10 + 86_400),
            actor_id: Some("break".to_string()),
        })
        .await
        .unwrap();
    // Same Delivery, same Sink, reset to delivering with a fresh deadline.
    assert_eq!(delivery.id, delivery_id);
    assert_eq!(delivery.sink_name, "journal");
    assert_eq!(delivery.status, DeliveryStatus::Delivering);
    assert!(delivery.completed_at.is_none());
    assert!(delivery.latest_error.is_none());
    assert_eq!(delivery.retry_deadline_at, Some(at(10 + 86_400)));

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Delivering);
    assert!(recording.latest_error.is_none());

    let events = store
        .list_audit_events_for_recording("rec-1")
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "manual_retry_delivery")
    );
}

#[tokio::test]
async fn recovery_closes_an_abandoned_transcription_attempt() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    store.claim_received_for_transcription(at(1)).await.unwrap();
    store
        .start_transcription_attempt("att-1", "rec-1", at(1))
        .await
        .unwrap()
        .expect("claim the attempt");

    // While the attempt is in flight the Recording is neither re-claimable nor a
    // retry candidate: it would be stranded if the process stopped now.
    assert!(
        store
            .transcription_retry_candidates()
            .await
            .unwrap()
            .is_empty()
    );

    let report = store.recover_abandoned_attempts(at(10)).await.unwrap();
    assert_eq!(report.transcription_attempts, 1);
    assert_eq!(report.reverted_recordings, 0);
    assert_eq!(report.delivery_attempts, 0);
    assert!(report.has_changes());

    // The attempt is now finished, so the normal retry path picks the Recording
    // up with backoff measured from the recovery time.
    let candidates = store.transcription_retry_candidates().await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].recording.id, "rec-1");
    assert_eq!(candidates[0].last_attempt_number, 1);
    assert_eq!(candidates[0].last_attempt_finished_at, at(10));

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Transcribing);
    assert!(recording.latest_error.is_some());
}

#[tokio::test]
async fn recovery_reverts_a_recording_claimed_without_an_attempt() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();
    // Claimed for Transcription, but the process stopped before the attempt row
    // was inserted.
    store.claim_received_for_transcription(at(1)).await.unwrap();

    let report = store.recover_abandoned_attempts(at(10)).await.unwrap();
    assert_eq!(report.reverted_recordings, 1);
    assert_eq!(report.transcription_attempts, 0);

    // Back to `received`, so the claim path picks it up again.
    let reclaimed = store
        .claim_received_for_transcription(at(11))
        .await
        .unwrap()
        .expect("recording is claimable again");
    assert_eq!(reclaimed.id, "rec-1");
}

#[tokio::test]
async fn recovery_closes_an_abandoned_delivery_attempt() {
    let (_dir, store) = fresh_store().await;
    let delivery_id = deliver_setup(&store, "rec-1").await;
    store
        .start_delivery_attempt("att-1", &delivery_id, at(4))
        .await
        .unwrap()
        .expect("claim the delivery attempt");

    // In flight: excluded from the delivery work queue until recovered.
    assert!(store.delivery_candidates().await.unwrap().is_empty());

    let report = store.recover_abandoned_attempts(at(20)).await.unwrap();
    assert_eq!(report.delivery_attempts, 1);
    assert_eq!(report.transcription_attempts, 0);

    let candidates = store.delivery_candidates().await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].delivery.id, delivery_id);
    assert_eq!(candidates[0].last_attempt_finished_at, Some(at(20)));
}

#[tokio::test]
async fn recovery_is_a_noop_without_abandoned_work() {
    let (_dir, store) = fresh_store().await;
    store
        .create_recording(new_recording("rec-1", "litewatch-main", "cr-1"))
        .await
        .unwrap();

    let report = store.recover_abandoned_attempts(at(10)).await.unwrap();
    assert!(!report.has_changes());
    assert_eq!(report.transcription_attempts, 0);
    assert_eq!(report.reverted_recordings, 0);
    assert_eq!(report.delivery_attempts, 0);

    let recording = store.get_recording("rec-1").await.unwrap().unwrap();
    assert_eq!(recording.status, RecordingStatus::Received);
}
