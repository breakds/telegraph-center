-- Initial schema for Telegraph Center v1 state.
-- Timestamps are RFC3339 text; JSON payloads are text. Foreign keys are
-- enabled at the connection level.

CREATE TABLE recordings (
    id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    client_recording_id TEXT NOT NULL,
    status TEXT NOT NULL,
    original_filename TEXT,
    blob_path TEXT,
    audio_size_bytes INTEGER,
    audio_duration_ms INTEGER,
    sample_rate_hz INTEGER,
    channels INTEGER,
    bits_per_sample INTEGER,
    tags_json TEXT NOT NULL,
    recorded_at TEXT,
    received_at TEXT NOT NULL,
    selected_sink_name TEXT,
    latest_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (client_id, client_recording_id)
);

CREATE INDEX idx_recordings_status ON recordings (status);
CREATE INDEX idx_recordings_received_at ON recordings (received_at);

CREATE TABLE transcripts (
    recording_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    text TEXT NOT NULL,
    raw_json TEXT NOT NULL,
    provider_file_id TEXT,
    provider_transcription_id TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (recording_id) REFERENCES recordings (id)
);

CREATE TABLE transcription_attempts (
    id TEXT PRIMARY KEY,
    recording_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL,
    retryable INTEGER NOT NULL,
    error_code TEXT,
    error_message TEXT,
    FOREIGN KEY (recording_id) REFERENCES recordings (id)
);

CREATE INDEX idx_transcription_attempts_recording
    ON transcription_attempts (recording_id);

CREATE TABLE deliveries (
    id TEXT PRIMARY KEY,
    recording_id TEXT NOT NULL,
    sink_name TEXT NOT NULL,
    status TEXT NOT NULL,
    selected_at TEXT NOT NULL,
    completed_at TEXT,
    retry_deadline_at TEXT,
    latest_error TEXT,
    UNIQUE (recording_id),
    FOREIGN KEY (recording_id) REFERENCES recordings (id)
);

CREATE TABLE delivery_attempts (
    id TEXT PRIMARY KEY,
    delivery_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL,
    http_status INTEGER,
    retryable INTEGER NOT NULL,
    error_message TEXT,
    FOREIGN KEY (delivery_id) REFERENCES deliveries (id)
);

CREATE INDEX idx_delivery_attempts_delivery
    ON delivery_attempts (delivery_id);

CREATE TABLE operator_sessions (
    session_hash TEXT PRIMARY KEY,
    operator_username TEXT NOT NULL,
    csrf_token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    idle_expires_at TEXT NOT NULL,
    absolute_expires_at TEXT NOT NULL,
    revoked_at TEXT
);

CREATE TABLE login_failures (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    remote_ip TEXT NOT NULL,
    failed_at TEXT NOT NULL
);

CREATE INDEX idx_login_failures_failed_at ON login_failures (failed_at);

CREATE TABLE audit_events (
    id TEXT PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor_id TEXT,
    event_type TEXT NOT NULL,
    recording_id TEXT,
    details_json TEXT NOT NULL
);

CREATE INDEX idx_audit_events_recording ON audit_events (recording_id);
