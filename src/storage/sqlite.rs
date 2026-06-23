//! SQLite-backed implementation of the storage layer.
//!
//! All SQL is explicit (no ORM). Timestamps are stored as RFC3339 text and
//! JSON payloads as text for readability and portability. Foreign keys are
//! enabled per connection.

use std::path::Path;
use std::time::Duration;

use sqlx::SqliteConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use time::OffsetDateTime;

use crate::domain::{
    AuditEvent, Delivery, DeliveryAttempt, DeliveryStatus, OperatorSession, Recording,
    RecordingStatus, Tags, Transcript, TranscriptionAttempt, timestamp,
};

use super::{
    NewAuditEvent, NewDelivery, NewDeliveryAttempt, NewLoginFailure, NewOperatorSession,
    NewRecording, NewTranscript, NewTranscriptionAttempt, RecordingCreation, StorageError,
};

/// A SQLite-backed store. Cloning shares the underlying connection pool.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path`, enable foreign keys,
    /// and run migrations.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        sqlx::migrate!().run(&pool).await?;

        Ok(Self { pool })
    }

    /// The underlying connection pool, for advanced use.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // -- Recordings ---------------------------------------------------------

    /// Create a Recording, or return the existing one for the same
    /// `(client_id, client_recording_id)` idempotency key.
    pub async fn create_recording(
        &self,
        new: NewRecording,
    ) -> Result<RecordingCreation, StorageError> {
        let received = timestamp::format(new.received_at);
        let recorded = new.recorded_at.map(timestamp::format);
        let tags_json = new.tags.to_json();

        let result = sqlx::query(
            "INSERT INTO recordings (
                 id, client_id, client_recording_id, status,
                 original_filename, blob_path,
                 audio_size_bytes, audio_duration_ms, sample_rate_hz, channels, bits_per_sample,
                 tags_json, recorded_at, received_at,
                 selected_sink_name, latest_error, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)
             ON CONFLICT(client_id, client_recording_id) DO NOTHING",
        )
        .bind(&new.id)
        .bind(&new.client_id)
        .bind(&new.client_recording_id)
        .bind(RecordingStatus::Received.as_str())
        .bind(new.original_filename.as_deref())
        .bind(new.blob_path.as_deref())
        .bind(new.audio_size_bytes)
        .bind(new.audio_duration_ms)
        .bind(new.sample_rate_hz)
        .bind(new.channels)
        .bind(new.bits_per_sample)
        .bind(&tags_json)
        .bind(recorded.as_deref())
        .bind(&received)
        .bind(&received)
        .bind(&received)
        .execute(&self.pool)
        .await?;

        let recording = self
            .get_recording_by_idempotency(&new.client_id, &new.client_recording_id)
            .await?
            .ok_or_else(|| StorageError::RecordingNotFound(new.id.clone()))?;

        Ok(RecordingCreation {
            recording,
            created: result.rows_affected() == 1,
        })
    }

    /// Fetch a Recording by its server identifier.
    pub async fn get_recording(&self, id: &str) -> Result<Option<Recording>, StorageError> {
        let row = sqlx::query_as::<_, RecordingRow>("SELECT * FROM recordings WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(RecordingRow::into_domain).transpose()
    }

    /// Fetch a Recording by its idempotency key.
    pub async fn get_recording_by_idempotency(
        &self,
        client_id: &str,
        client_recording_id: &str,
    ) -> Result<Option<Recording>, StorageError> {
        let row = sqlx::query_as::<_, RecordingRow>(
            "SELECT * FROM recordings WHERE client_id = ? AND client_recording_id = ?",
        )
        .bind(client_id)
        .bind(client_recording_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(RecordingRow::into_domain).transpose()
    }

    /// Apply a Recording status transition, validating that it is permitted.
    pub async fn update_recording_status(
        &self,
        id: &str,
        next: RecordingStatus,
        at: OffsetDateTime,
    ) -> Result<Recording, StorageError> {
        let mut tx = self.pool.begin().await?;
        guarded_transition(&mut tx, id, next, at).await?;
        tx.commit().await?;
        self.require_recording(id).await
    }

    /// Move a Recording from `routing` to `backlogged` (no Sink selected).
    pub async fn mark_backlogged(
        &self,
        id: &str,
        at: OffsetDateTime,
    ) -> Result<Recording, StorageError> {
        self.update_recording_status(id, RecordingStatus::Backlogged, at)
            .await
    }

    // -- Transcripts --------------------------------------------------------

    /// Store a Transcript and move the Recording from `transcribing` to
    /// `routing` in a single transaction.
    pub async fn store_transcript(&self, new: NewTranscript) -> Result<Transcript, StorageError> {
        let created = timestamp::format(new.created_at);
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO transcripts (
                 recording_id, provider, text, raw_json,
                 provider_file_id, provider_transcription_id, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new.recording_id)
        .bind(&new.provider)
        .bind(&new.text)
        .bind(&new.raw_json)
        .bind(new.provider_file_id.as_deref())
        .bind(new.provider_transcription_id.as_deref())
        .bind(&created)
        .execute(&mut *tx)
        .await?;

        guarded_transition(
            &mut tx,
            &new.recording_id,
            RecordingStatus::Routing,
            new.created_at,
        )
        .await?;
        tx.commit().await?;

        self.require_transcript(&new.recording_id).await
    }

    /// Fetch a Recording's Transcript, if any.
    pub async fn get_transcript(
        &self,
        recording_id: &str,
    ) -> Result<Option<Transcript>, StorageError> {
        let row =
            sqlx::query_as::<_, TranscriptRow>("SELECT * FROM transcripts WHERE recording_id = ?")
                .bind(recording_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(TranscriptRow::into_domain).transpose()
    }

    // -- Transcription attempts ---------------------------------------------

    /// Append a Transcription Attempt row.
    pub async fn insert_transcription_attempt(
        &self,
        new: NewTranscriptionAttempt,
    ) -> Result<TranscriptionAttempt, StorageError> {
        sqlx::query(
            "INSERT INTO transcription_attempts (
                 id, recording_id, attempt_number, started_at, finished_at,
                 status, retryable, error_code, error_message
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new.id)
        .bind(&new.recording_id)
        .bind(new.attempt_number)
        .bind(timestamp::format(new.started_at))
        .bind(new.finished_at.map(timestamp::format))
        .bind(&new.status)
        .bind(new.retryable)
        .bind(new.error_code.as_deref())
        .bind(new.error_message.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(TranscriptionAttempt {
            id: new.id,
            recording_id: new.recording_id,
            attempt_number: new.attempt_number,
            started_at: new.started_at,
            finished_at: new.finished_at,
            status: new.status,
            retryable: new.retryable,
            error_code: new.error_code,
            error_message: new.error_message,
        })
    }

    /// List a Recording's Transcription Attempts, oldest first.
    pub async fn list_transcription_attempts(
        &self,
        recording_id: &str,
    ) -> Result<Vec<TranscriptionAttempt>, StorageError> {
        let rows = sqlx::query_as::<_, TranscriptionAttemptRow>(
            "SELECT * FROM transcription_attempts WHERE recording_id = ? ORDER BY attempt_number",
        )
        .bind(recording_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(TranscriptionAttemptRow::into_domain)
            .collect()
    }

    // -- Deliveries ---------------------------------------------------------

    /// Select a Sink: create the one-per-Recording Delivery, record the
    /// selected Sink name, and move the Recording to `delivering`.
    pub async fn select_sink(&self, new: NewDelivery) -> Result<Delivery, StorageError> {
        let selected = timestamp::format(new.selected_at);
        let deadline = new.retry_deadline_at.map(timestamp::format);
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO deliveries (
                 id, recording_id, sink_name, status,
                 selected_at, completed_at, retry_deadline_at, latest_error
             ) VALUES (?, ?, ?, ?, ?, NULL, ?, NULL)",
        )
        .bind(&new.id)
        .bind(&new.recording_id)
        .bind(&new.sink_name)
        .bind(DeliveryStatus::Delivering.as_str())
        .bind(&selected)
        .bind(deadline.as_deref())
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE recordings SET selected_sink_name = ? WHERE id = ?")
            .bind(&new.sink_name)
            .bind(&new.recording_id)
            .execute(&mut *tx)
            .await?;

        guarded_transition(
            &mut tx,
            &new.recording_id,
            RecordingStatus::Delivering,
            new.selected_at,
        )
        .await?;
        tx.commit().await?;

        self.require_delivery(&new.id).await
    }

    /// Fetch a Delivery by its identifier.
    pub async fn get_delivery(&self, id: &str) -> Result<Option<Delivery>, StorageError> {
        let row = sqlx::query_as::<_, DeliveryRow>("SELECT * FROM deliveries WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(DeliveryRow::into_domain).transpose()
    }

    /// Fetch the Delivery for a Recording, if a Sink was selected.
    pub async fn get_delivery_for_recording(
        &self,
        recording_id: &str,
    ) -> Result<Option<Delivery>, StorageError> {
        let row =
            sqlx::query_as::<_, DeliveryRow>("SELECT * FROM deliveries WHERE recording_id = ?")
                .bind(recording_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(DeliveryRow::into_domain).transpose()
    }

    /// Mark a Delivery delivered and move the Recording to `delivered`.
    pub async fn mark_delivery_delivered(
        &self,
        delivery_id: &str,
        at: OffsetDateTime,
    ) -> Result<Delivery, StorageError> {
        self.complete_delivery(delivery_id, DeliveryStatus::Delivered, at, None)
            .await
    }

    /// Mark a Delivery failed and move the Recording to `delivery_failed`.
    ///
    /// This is the failed-Delivery path for an already selected Sink; it is
    /// distinct from the Backlog, which means no Sink was ever selected.
    pub async fn mark_delivery_failed(
        &self,
        delivery_id: &str,
        at: OffsetDateTime,
        error: &str,
    ) -> Result<Delivery, StorageError> {
        self.complete_delivery(delivery_id, DeliveryStatus::DeliveryFailed, at, Some(error))
            .await
    }

    async fn complete_delivery(
        &self,
        delivery_id: &str,
        status: DeliveryStatus,
        at: OffsetDateTime,
        error: Option<&str>,
    ) -> Result<Delivery, StorageError> {
        let recording_status = match status {
            DeliveryStatus::Delivered => RecordingStatus::Delivered,
            DeliveryStatus::DeliveryFailed => RecordingStatus::DeliveryFailed,
            DeliveryStatus::Delivering => RecordingStatus::Delivering,
        };
        let completed = timestamp::format(at);
        let mut tx = self.pool.begin().await?;

        let recording_id: Option<String> =
            sqlx::query_scalar("SELECT recording_id FROM deliveries WHERE id = ?")
                .bind(delivery_id)
                .fetch_optional(&mut *tx)
                .await?;
        let recording_id =
            recording_id.ok_or_else(|| StorageError::DeliveryNotFound(delivery_id.to_string()))?;

        sqlx::query(
            "UPDATE deliveries SET status = ?, completed_at = ?, latest_error = ? WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(&completed)
        .bind(error)
        .bind(delivery_id)
        .execute(&mut *tx)
        .await?;

        if let Some(message) = error {
            sqlx::query("UPDATE recordings SET latest_error = ? WHERE id = ?")
                .bind(message)
                .bind(&recording_id)
                .execute(&mut *tx)
                .await?;
        }

        guarded_transition(&mut tx, &recording_id, recording_status, at).await?;
        tx.commit().await?;

        self.require_delivery(delivery_id).await
    }

    // -- Delivery attempts --------------------------------------------------

    /// Append a Delivery Attempt row.
    pub async fn insert_delivery_attempt(
        &self,
        new: NewDeliveryAttempt,
    ) -> Result<DeliveryAttempt, StorageError> {
        sqlx::query(
            "INSERT INTO delivery_attempts (
                 id, delivery_id, attempt_number, started_at, finished_at,
                 status, http_status, retryable, error_message
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new.id)
        .bind(&new.delivery_id)
        .bind(new.attempt_number)
        .bind(timestamp::format(new.started_at))
        .bind(new.finished_at.map(timestamp::format))
        .bind(&new.status)
        .bind(new.http_status)
        .bind(new.retryable)
        .bind(new.error_message.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(DeliveryAttempt {
            id: new.id,
            delivery_id: new.delivery_id,
            attempt_number: new.attempt_number,
            started_at: new.started_at,
            finished_at: new.finished_at,
            status: new.status,
            http_status: new.http_status,
            retryable: new.retryable,
            error_message: new.error_message,
        })
    }

    /// List a Delivery's Attempts, oldest first.
    pub async fn list_delivery_attempts(
        &self,
        delivery_id: &str,
    ) -> Result<Vec<DeliveryAttempt>, StorageError> {
        let rows = sqlx::query_as::<_, DeliveryAttemptRow>(
            "SELECT * FROM delivery_attempts WHERE delivery_id = ? ORDER BY attempt_number",
        )
        .bind(delivery_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(DeliveryAttemptRow::into_domain)
            .collect()
    }

    // -- Operator sessions --------------------------------------------------

    /// Create an Operator Session row.
    pub async fn create_session(
        &self,
        new: NewOperatorSession,
    ) -> Result<OperatorSession, StorageError> {
        sqlx::query(
            "INSERT INTO operator_sessions (
                 session_hash, operator_username, csrf_token_hash,
                 created_at, last_seen_at, idle_expires_at, absolute_expires_at, revoked_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(&new.session_hash)
        .bind(&new.operator_username)
        .bind(&new.csrf_token_hash)
        .bind(timestamp::format(new.created_at))
        .bind(timestamp::format(new.last_seen_at))
        .bind(timestamp::format(new.idle_expires_at))
        .bind(timestamp::format(new.absolute_expires_at))
        .execute(&self.pool)
        .await?;

        Ok(OperatorSession {
            session_hash: new.session_hash,
            operator_username: new.operator_username,
            csrf_token_hash: new.csrf_token_hash,
            created_at: new.created_at,
            last_seen_at: new.last_seen_at,
            idle_expires_at: new.idle_expires_at,
            absolute_expires_at: new.absolute_expires_at,
            revoked_at: None,
        })
    }

    /// Fetch an Operator Session by its token hash.
    pub async fn get_session(
        &self,
        session_hash: &str,
    ) -> Result<Option<OperatorSession>, StorageError> {
        let row = sqlx::query_as::<_, OperatorSessionRow>(
            "SELECT * FROM operator_sessions WHERE session_hash = ?",
        )
        .bind(session_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(OperatorSessionRow::into_domain).transpose()
    }

    /// Revoke an Operator Session, returning whether a row was updated.
    pub async fn revoke_session(
        &self,
        session_hash: &str,
        at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            "UPDATE operator_sessions SET revoked_at = ? WHERE session_hash = ? AND revoked_at IS NULL",
        )
        .bind(timestamp::format(at))
        .bind(session_hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    // -- Login failures -----------------------------------------------------

    /// Record a failed login attempt.
    pub async fn record_login_failure(&self, new: NewLoginFailure) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO login_failures (id, username, remote_ip, failed_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&new.id)
        .bind(&new.username)
        .bind(&new.remote_ip)
        .bind(timestamp::format(new.failed_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count recent login failures matching a username or remote IP since a
    /// cutoff time.
    pub async fn count_login_failures_since(
        &self,
        username: &str,
        remote_ip: &str,
        since: OffsetDateTime,
    ) -> Result<i64, StorageError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM login_failures
             WHERE (username = ? OR remote_ip = ?) AND failed_at >= ?",
        )
        .bind(username)
        .bind(remote_ip)
        .bind(timestamp::format(since))
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    // -- Audit --------------------------------------------------------------

    /// Append an audit event.
    pub async fn insert_audit_event(&self, new: NewAuditEvent) -> Result<AuditEvent, StorageError> {
        sqlx::query(
            "INSERT INTO audit_events (
                 id, occurred_at, actor_kind, actor_id, event_type, recording_id, details_json
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new.id)
        .bind(timestamp::format(new.occurred_at))
        .bind(&new.actor_kind)
        .bind(new.actor_id.as_deref())
        .bind(&new.event_type)
        .bind(new.recording_id.as_deref())
        .bind(&new.details_json)
        .execute(&self.pool)
        .await?;

        Ok(AuditEvent {
            id: new.id,
            occurred_at: new.occurred_at,
            actor_kind: new.actor_kind,
            actor_id: new.actor_id,
            event_type: new.event_type,
            recording_id: new.recording_id,
            details_json: new.details_json,
        })
    }

    /// List audit events for a Recording, oldest first.
    pub async fn list_audit_events_for_recording(
        &self,
        recording_id: &str,
    ) -> Result<Vec<AuditEvent>, StorageError> {
        let rows = sqlx::query_as::<_, AuditEventRow>(
            "SELECT * FROM audit_events WHERE recording_id = ? ORDER BY occurred_at",
        )
        .bind(recording_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(AuditEventRow::into_domain).collect()
    }

    // -- Internal helpers ---------------------------------------------------

    async fn require_recording(&self, id: &str) -> Result<Recording, StorageError> {
        self.get_recording(id)
            .await?
            .ok_or_else(|| StorageError::RecordingNotFound(id.to_string()))
    }

    async fn require_transcript(&self, recording_id: &str) -> Result<Transcript, StorageError> {
        self.get_transcript(recording_id)
            .await?
            .ok_or_else(|| StorageError::RecordingNotFound(recording_id.to_string()))
    }

    async fn require_delivery(&self, id: &str) -> Result<Delivery, StorageError> {
        self.get_delivery(id)
            .await?
            .ok_or_else(|| StorageError::DeliveryNotFound(id.to_string()))
    }
}

/// Load the current status of a Recording within a transaction, validate the
/// requested transition, and apply it.
async fn guarded_transition(
    conn: &mut SqliteConnection,
    id: &str,
    next: RecordingStatus,
    at: OffsetDateTime,
) -> Result<(), StorageError> {
    let current: Option<String> = sqlx::query_scalar("SELECT status FROM recordings WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let current = current.ok_or_else(|| StorageError::RecordingNotFound(id.to_string()))?;
    let current =
        RecordingStatus::parse(&current).map_err(|e| StorageError::Corrupt(e.to_string()))?;

    current.transition_to(next)?;

    sqlx::query("UPDATE recordings SET status = ?, updated_at = ? WHERE id = ?")
        .bind(next.as_str())
        .bind(timestamp::format(at))
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Row types: the DB representation, converted into domain types explicitly.
// ---------------------------------------------------------------------------

fn parse_ts(value: &str) -> Result<OffsetDateTime, StorageError> {
    timestamp::parse(value)
        .map_err(|e| StorageError::Corrupt(format!("invalid timestamp {value:?}: {e}")))
}

fn parse_ts_opt(value: Option<String>) -> Result<Option<OffsetDateTime>, StorageError> {
    value.as_deref().map(parse_ts).transpose()
}

#[derive(sqlx::FromRow)]
struct RecordingRow {
    id: String,
    client_id: String,
    client_recording_id: String,
    status: String,
    original_filename: Option<String>,
    blob_path: Option<String>,
    audio_size_bytes: Option<i64>,
    audio_duration_ms: Option<i64>,
    sample_rate_hz: Option<i64>,
    channels: Option<i64>,
    bits_per_sample: Option<i64>,
    tags_json: String,
    recorded_at: Option<String>,
    received_at: String,
    selected_sink_name: Option<String>,
    latest_error: Option<String>,
    created_at: String,
    updated_at: String,
}

impl RecordingRow {
    fn into_domain(self) -> Result<Recording, StorageError> {
        Ok(Recording {
            id: self.id,
            client_id: self.client_id,
            client_recording_id: self.client_recording_id,
            status: RecordingStatus::parse(&self.status)
                .map_err(|e| StorageError::Corrupt(e.to_string()))?,
            original_filename: self.original_filename,
            blob_path: self.blob_path,
            audio_size_bytes: self.audio_size_bytes,
            audio_duration_ms: self.audio_duration_ms,
            sample_rate_hz: self.sample_rate_hz,
            channels: self.channels,
            bits_per_sample: self.bits_per_sample,
            tags: Tags::from_json(&self.tags_json)
                .map_err(|e| StorageError::Corrupt(e.to_string()))?,
            recorded_at: parse_ts_opt(self.recorded_at)?,
            received_at: parse_ts(&self.received_at)?,
            selected_sink_name: self.selected_sink_name,
            latest_error: self.latest_error,
            created_at: parse_ts(&self.created_at)?,
            updated_at: parse_ts(&self.updated_at)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TranscriptRow {
    recording_id: String,
    provider: String,
    text: String,
    raw_json: String,
    provider_file_id: Option<String>,
    provider_transcription_id: Option<String>,
    created_at: String,
}

impl TranscriptRow {
    fn into_domain(self) -> Result<Transcript, StorageError> {
        Ok(Transcript {
            recording_id: self.recording_id,
            provider: self.provider,
            text: self.text,
            raw_json: self.raw_json,
            provider_file_id: self.provider_file_id,
            provider_transcription_id: self.provider_transcription_id,
            created_at: parse_ts(&self.created_at)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TranscriptionAttemptRow {
    id: String,
    recording_id: String,
    attempt_number: i64,
    started_at: String,
    finished_at: Option<String>,
    status: String,
    retryable: bool,
    error_code: Option<String>,
    error_message: Option<String>,
}

impl TranscriptionAttemptRow {
    fn into_domain(self) -> Result<TranscriptionAttempt, StorageError> {
        Ok(TranscriptionAttempt {
            id: self.id,
            recording_id: self.recording_id,
            attempt_number: self.attempt_number,
            started_at: parse_ts(&self.started_at)?,
            finished_at: parse_ts_opt(self.finished_at)?,
            status: self.status,
            retryable: self.retryable,
            error_code: self.error_code,
            error_message: self.error_message,
        })
    }
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    id: String,
    recording_id: String,
    sink_name: String,
    status: String,
    selected_at: String,
    completed_at: Option<String>,
    retry_deadline_at: Option<String>,
    latest_error: Option<String>,
}

impl DeliveryRow {
    fn into_domain(self) -> Result<Delivery, StorageError> {
        Ok(Delivery {
            id: self.id,
            recording_id: self.recording_id,
            sink_name: self.sink_name,
            status: DeliveryStatus::parse(&self.status)
                .map_err(|e| StorageError::Corrupt(e.to_string()))?,
            selected_at: parse_ts(&self.selected_at)?,
            completed_at: parse_ts_opt(self.completed_at)?,
            retry_deadline_at: parse_ts_opt(self.retry_deadline_at)?,
            latest_error: self.latest_error,
        })
    }
}

#[derive(sqlx::FromRow)]
struct DeliveryAttemptRow {
    id: String,
    delivery_id: String,
    attempt_number: i64,
    started_at: String,
    finished_at: Option<String>,
    status: String,
    http_status: Option<i64>,
    retryable: bool,
    error_message: Option<String>,
}

impl DeliveryAttemptRow {
    fn into_domain(self) -> Result<DeliveryAttempt, StorageError> {
        Ok(DeliveryAttempt {
            id: self.id,
            delivery_id: self.delivery_id,
            attempt_number: self.attempt_number,
            started_at: parse_ts(&self.started_at)?,
            finished_at: parse_ts_opt(self.finished_at)?,
            status: self.status,
            http_status: self.http_status,
            retryable: self.retryable,
            error_message: self.error_message,
        })
    }
}

#[derive(sqlx::FromRow)]
struct OperatorSessionRow {
    session_hash: String,
    operator_username: String,
    csrf_token_hash: String,
    created_at: String,
    last_seen_at: String,
    idle_expires_at: String,
    absolute_expires_at: String,
    revoked_at: Option<String>,
}

impl OperatorSessionRow {
    fn into_domain(self) -> Result<OperatorSession, StorageError> {
        Ok(OperatorSession {
            session_hash: self.session_hash,
            operator_username: self.operator_username,
            csrf_token_hash: self.csrf_token_hash,
            created_at: parse_ts(&self.created_at)?,
            last_seen_at: parse_ts(&self.last_seen_at)?,
            idle_expires_at: parse_ts(&self.idle_expires_at)?,
            absolute_expires_at: parse_ts(&self.absolute_expires_at)?,
            revoked_at: parse_ts_opt(self.revoked_at)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AuditEventRow {
    id: String,
    occurred_at: String,
    actor_kind: String,
    actor_id: Option<String>,
    event_type: String,
    recording_id: Option<String>,
    details_json: String,
}

impl AuditEventRow {
    fn into_domain(self) -> Result<AuditEvent, StorageError> {
        Ok(AuditEvent {
            id: self.id,
            occurred_at: parse_ts(&self.occurred_at)?,
            actor_kind: self.actor_kind,
            actor_id: self.actor_id,
            event_type: self.event_type,
            recording_id: self.recording_id,
            details_json: self.details_json,
        })
    }
}
