//! Soniox adapter tests using a fake transport (no network, no credentials).
//!
//! An optional real-Soniox test lives at the bottom, gated behind `#[ignore]`
//! and the `SONIOX_API_KEY` environment variable.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::macros::datetime;

use telegraph_center::config::SonioxConfig;
use telegraph_center::domain::{Recording, RecordingStatus, Tags};
use telegraph_center::soniox::{
    CreateTranscriptionRequest, SonioxBuildError, SonioxError, SonioxTranscriber, SonioxTransport,
};
use telegraph_center::workers::{Transcriber, TranscriptionRequest, WorkerFailure};

const WHEN: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);

// --- Fake transport --------------------------------------------------------

#[derive(Default)]
struct Calls {
    statuses: VecDeque<Result<Value, SonioxError>>,
    created: Option<CapturedCreate>,
    deleted_transcriptions: Vec<String>,
    deleted_files: Vec<String>,
}

struct CapturedCreate {
    model: String,
    file_id: String,
    language_hints: Vec<String>,
    enable_speaker_diarization: bool,
    enable_language_identification: bool,
    client_reference_id: String,
}

struct FakeTransport {
    upload: Result<String, SonioxError>,
    create: Result<String, SonioxError>,
    transcript: Result<Value, SonioxError>,
    calls: Arc<Mutex<Calls>>,
}

impl FakeTransport {
    fn new(statuses: Vec<Result<Value, SonioxError>>) -> Self {
        Self {
            upload: Ok("file-1".to_string()),
            create: Ok("tr-1".to_string()),
            transcript: Ok(no_speaker_transcript()),
            calls: Arc::new(Mutex::new(Calls {
                statuses: statuses.into(),
                ..Calls::default()
            })),
        }
    }

    fn calls(&self) -> Arc<Mutex<Calls>> {
        self.calls.clone()
    }
}

impl SonioxTransport for FakeTransport {
    async fn upload_file(
        &self,
        _audio: Vec<u8>,
        _filename: &str,
        _client_reference_id: &str,
    ) -> Result<String, SonioxError> {
        self.upload.clone()
    }

    async fn create_transcription(
        &self,
        request: &CreateTranscriptionRequest<'_>,
    ) -> Result<String, SonioxError> {
        self.calls.lock().unwrap().created = Some(CapturedCreate {
            model: request.model.to_string(),
            file_id: request.file_id.to_string(),
            language_hints: request.language_hints.to_vec(),
            enable_speaker_diarization: request.enable_speaker_diarization,
            enable_language_identification: request.enable_language_identification,
            client_reference_id: request.client_reference_id.to_string(),
        });
        self.create.clone()
    }

    async fn get_transcription(&self, _transcription_id: &str) -> Result<Value, SonioxError> {
        self.calls
            .lock()
            .unwrap()
            .statuses
            .pop_front()
            .unwrap_or_else(|| Err(SonioxError::Decode("no scripted status".to_string())))
    }

    async fn get_transcript(&self, _transcription_id: &str) -> Result<Value, SonioxError> {
        self.transcript.clone()
    }

    async fn delete_transcription(&self, transcription_id: &str) -> Result<(), SonioxError> {
        self.calls
            .lock()
            .unwrap()
            .deleted_transcriptions
            .push(transcription_id.to_string());
        Ok(())
    }

    async fn delete_file(&self, file_id: &str) -> Result<(), SonioxError> {
        self.calls
            .lock()
            .unwrap()
            .deleted_files
            .push(file_id.to_string());
        Ok(())
    }
}

// --- Helpers ---------------------------------------------------------------

fn config() -> SonioxConfig {
    SonioxConfig {
        api_key_env: "UNUSED".to_string(),
        model: "stt-async-v5".to_string(),
        language_hints: vec!["en".to_string()],
        enable_speaker_diarization: true,
        enable_language_identification: false,
    }
}

fn recording(id: &str) -> Recording {
    Recording {
        id: id.to_string(),
        client_id: "litewatch-main".to_string(),
        client_recording_id: id.to_string(),
        status: RecordingStatus::Transcribing,
        original_filename: None,
        blob_path: Some("recordings/x.wav".to_string()),
        audio_size_bytes: Some(10),
        audio_duration_ms: Some(1_000),
        sample_rate_hz: Some(16_000),
        channels: Some(1),
        bits_per_sample: Some(16),
        tags: Tags::default(),
        recorded_at: None,
        received_at: WHEN,
        selected_sink_name: None,
        latest_error: None,
        created_at: WHEN,
        updated_at: WHEN,
    }
}

fn completed_status() -> Value {
    json!({ "id": "tr-1", "status": "completed", "model": "stt-async-v5" })
}

fn no_speaker_transcript() -> Value {
    json!({ "text": "hello world", "tokens": [{ "text": "hello" }, { "text": " world" }] })
}

async fn audio_file() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("audio.wav");
    tokio::fs::write(&path, b"RIFF....WAVEfake").await.unwrap();
    (dir, path)
}

async fn transcribe(
    fake: FakeTransport,
    rec: &Recording,
    path: PathBuf,
) -> Result<telegraph_center::workers::TranscriptionOutput, WorkerFailure> {
    let transcriber = SonioxTranscriber::with_transport(config(), fake, Duration::ZERO, 10);
    transcriber
        .transcribe(TranscriptionRequest {
            recording: rec,
            audio_path: path,
        })
        .await
}

// --- Success paths ---------------------------------------------------------

#[tokio::test]
async fn successful_flow_returns_output_and_cleans_up() {
    let (_dir, path) = audio_file().await;
    let fake = FakeTransport::new(vec![Ok(completed_status())]);
    let calls = fake.calls();

    let output = transcribe(fake, &recording("rec-1"), path).await.unwrap();

    assert_eq!(output.provider, "soniox");
    assert_eq!(output.text, "hello world");
    assert_eq!(output.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(output.provider_transcription_id.as_deref(), Some("tr-1"));

    // Raw JSON includes both the transcription status and the transcript.
    let raw: Value = serde_json::from_str(&output.raw_json).unwrap();
    assert_eq!(raw["transcription"]["status"], "completed");
    assert_eq!(raw["transcript"]["text"], "hello world");

    // Cleanup was attempted for both provider resources.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.deleted_transcriptions, vec!["tr-1".to_string()]);
    assert_eq!(calls.deleted_files, vec!["file-1".to_string()]);
}

#[tokio::test]
async fn builds_create_request_from_config() {
    let (_dir, path) = audio_file().await;
    let fake = FakeTransport::new(vec![Ok(completed_status())]);
    let calls = fake.calls();

    transcribe(fake, &recording("rec-42"), path).await.unwrap();

    let calls = calls.lock().unwrap();
    let created = calls.created.as_ref().expect("create was called");
    assert_eq!(created.model, "stt-async-v5");
    assert_eq!(created.file_id, "file-1");
    assert_eq!(created.language_hints, vec!["en".to_string()]);
    assert!(created.enable_speaker_diarization);
    assert!(!created.enable_language_identification);
    assert_eq!(created.client_reference_id, "rec-42");
}

#[tokio::test]
async fn polls_through_processing_until_completed() {
    let (_dir, path) = audio_file().await;
    let fake = FakeTransport::new(vec![
        Ok(json!({ "status": "queued" })),
        Ok(json!({ "status": "processing" })),
        Ok(completed_status()),
    ]);

    let output = transcribe(fake, &recording("rec-1"), path).await.unwrap();
    assert_eq!(output.text, "hello world");
}

#[tokio::test]
async fn renders_speaker_labels() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![Ok(completed_status())]);
    fake.transcript = Ok(json!({
        "text": "hi there",
        "tokens": [
            { "text": "hi", "speaker": 1 },
            { "text": " there", "speaker": 2 }
        ]
    }));

    let output = transcribe(fake, &recording("rec-1"), path).await.unwrap();
    assert_eq!(output.text, "Speaker 1: hi\nSpeaker 2: there");
}

// --- Failure paths ---------------------------------------------------------

#[tokio::test]
async fn upload_server_error_is_retryable() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![]);
    fake.upload = Err(SonioxError::Http {
        status: 503,
        error_type: None,
        message: None,
        request_id: None,
    });
    let calls = fake.calls();

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    assert!(matches!(failure, WorkerFailure::Retryable { .. }));
    // Nothing was created, so nothing is cleaned up.
    let calls = calls.lock().unwrap();
    assert!(calls.deleted_files.is_empty());
    assert!(calls.deleted_transcriptions.is_empty());
}

#[tokio::test]
async fn rate_limited_upload_is_retryable() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![]);
    fake.upload = Err(SonioxError::Http {
        status: 429,
        error_type: Some("limit_exceeded".to_string()),
        message: Some("rate limit exceeded: requests per minute".to_string()),
        request_id: None,
    });

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    assert!(matches!(failure, WorkerFailure::Retryable { .. }));
}

#[tokio::test]
async fn quota_capped_upload_is_terminal() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![]);
    fake.upload = Err(SonioxError::Http {
        status: 429,
        error_type: Some("limit_exceeded".to_string()),
        message: Some("total file size limit exceeded".to_string()),
        request_id: None,
    });

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    let WorkerFailure::Terminal { message, .. } = failure else {
        panic!("expected terminal failure for a persistent quota cap");
    };
    assert!(message.contains("total file size limit exceeded"));
}

#[tokio::test]
async fn transport_error_is_retryable() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![]);
    fake.upload = Err(SonioxError::Transport("connection reset".to_string()));

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    assert!(matches!(failure, WorkerFailure::Retryable { .. }));
}

#[tokio::test]
async fn create_invalid_request_is_terminal_and_deletes_file() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![]);
    fake.create = Err(SonioxError::Http {
        status: 400,
        error_type: Some("invalid_request".to_string()),
        message: Some("unsupported audio".to_string()),
        request_id: None,
    });
    let calls = fake.calls();

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    assert!(matches!(failure, WorkerFailure::Terminal { .. }));

    // The uploaded file is cleaned up; no transcription was created.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.deleted_files, vec!["file-1".to_string()]);
    assert!(calls.deleted_transcriptions.is_empty());
}

#[tokio::test]
async fn unauthenticated_is_terminal_without_leaking_key() {
    let (_dir, path) = audio_file().await;
    let mut fake = FakeTransport::new(vec![]);
    fake.upload = Err(SonioxError::Http {
        status: 401,
        error_type: Some("unauthenticated".to_string()),
        message: Some("invalid api key".to_string()),
        request_id: None,
    });

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    let WorkerFailure::Terminal { code, message } = failure else {
        panic!("expected terminal failure");
    };
    assert!(code.contains("401"));
    assert!(message.contains("HTTP 401"));
}

#[tokio::test]
async fn provider_status_error_is_terminal_with_message() {
    let (_dir, path) = audio_file().await;
    let fake = FakeTransport::new(vec![Ok(json!({
        "status": "error",
        "error_type": "audio_too_long",
        "error_message": "audio exceeds provider limit"
    }))]);
    let calls = fake.calls();

    let failure = transcribe(fake, &recording("rec-1"), path)
        .await
        .unwrap_err();
    let WorkerFailure::Terminal { code, message } = failure else {
        panic!("expected terminal failure");
    };
    assert!(code.contains("audio_too_long"));
    assert!(message.contains("audio exceeds provider limit"));

    // Provider resources are cleaned up after a provider error.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.deleted_transcriptions, vec!["tr-1".to_string()]);
    assert_eq!(calls.deleted_files, vec!["file-1".to_string()]);
}

#[tokio::test]
async fn missing_audio_file_is_terminal() {
    let fake = FakeTransport::new(vec![Ok(completed_status())]);
    let failure = transcribe(
        fake,
        &recording("rec-1"),
        PathBuf::from("/no/such/file.wav"),
    )
    .await
    .unwrap_err();
    let WorkerFailure::Terminal { code, .. } = failure else {
        panic!("expected terminal failure");
    };
    assert_eq!(code, "soniox_audio_unreadable");
}

// --- Construction / secret handling ----------------------------------------

#[test]
fn missing_api_key_is_rejected() {
    unsafe { std::env::remove_var("M4_MISSING_KEY") };
    let cfg = SonioxConfig {
        api_key_env: "M4_MISSING_KEY".to_string(),
        ..config()
    };
    let result = SonioxTranscriber::from_config(&cfg, reqwest::Client::new());
    assert!(matches!(result, Err(SonioxBuildError::MissingApiKey(_))));
}

#[test]
fn blank_api_key_is_rejected() {
    unsafe { std::env::set_var("M4_BLANK_KEY", "   ") };
    let cfg = SonioxConfig {
        api_key_env: "M4_BLANK_KEY".to_string(),
        ..config()
    };
    let result = SonioxTranscriber::from_config(&cfg, reqwest::Client::new());
    assert!(matches!(result, Err(SonioxBuildError::BlankApiKey(_))));
    unsafe { std::env::remove_var("M4_BLANK_KEY") };
}

#[test]
fn valid_api_key_builds_adapter() {
    unsafe { std::env::set_var("M4_OK_KEY", "test-key") };
    let cfg = SonioxConfig {
        api_key_env: "M4_OK_KEY".to_string(),
        ..config()
    };
    let result = SonioxTranscriber::from_config(&cfg, reqwest::Client::new());
    assert!(result.is_ok());
    unsafe { std::env::remove_var("M4_OK_KEY") };
}

// --- Optional real Soniox integration test ---------------------------------
//
// Run with real credentials only:
//   SONIOX_API_KEY=... cargo test --test soniox -- --ignored real_soniox
//
// It uploads a tiny generated WAV, transcribes it, and deletes the Soniox-side
// resources. It is never run by the default `cargo test`.

#[tokio::test]
#[ignore = "requires SONIOX_API_KEY and network access"]
async fn real_soniox_transcribes_a_tiny_wav() {
    let cfg = SonioxConfig {
        api_key_env: "SONIOX_API_KEY".to_string(),
        model: "stt-async-v5".to_string(),
        language_hints: vec!["en".to_string()],
        enable_speaker_diarization: false,
        enable_language_identification: false,
    };
    let transcriber = SonioxTranscriber::from_config(&cfg, reqwest::Client::new())
        .expect("SONIOX_API_KEY must be set to run this test");

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("silence.wav");
    tokio::fs::write(&path, one_second_silence_wav())
        .await
        .unwrap();

    let rec = recording("integration-test");
    let output = transcriber
        .transcribe(TranscriptionRequest {
            recording: &rec,
            audio_path: path,
        })
        .await
        .expect("real transcription should complete");
    assert_eq!(output.provider, "soniox");
}

/// One second of 16 kHz / 16-bit / mono silence as a WAV.
fn one_second_silence_wav() -> Vec<u8> {
    let sample_rate = 16_000u32;
    let channels = 1u16;
    let bits = 16u16;
    let data_len = sample_rate * u32::from(channels) * (u32::from(bits) / 8);
    let byte_rate = sample_rate * u32::from(channels) * (u32::from(bits) / 8);
    let block_align = channels * (bits / 8);
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&byte_rate.to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, data_len as usize));
    v
}
