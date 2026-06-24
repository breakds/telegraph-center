//! Soniox async Speech-to-Text adapter.
//!
//! [`SonioxTranscriber`] implements [`crate::workers::Transcriber`] by uploading
//! stored audio to the Soniox async Files API, creating a transcription,
//! polling until completion, fetching and rendering the transcript, and
//! attempting to delete the provider-side resources. HTTP lives behind the
//! [`SonioxTransport`] trait so normal tests run without network or credentials.
//! The API key is read from the environment at construction and is never logged
//! or stored in config.

pub mod client;
pub mod error;
pub mod render;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::config::SonioxConfig;
use crate::workers::{Transcriber, TranscriptionOutput, TranscriptionRequest, WorkerFailure};

pub use client::{CreateTranscriptionRequest, ReqwestTransport};
pub use error::SonioxError;

/// Default Soniox REST base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.soniox.com/v1";

/// Default poll interval between transcription status checks.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default maximum number of status polls before giving up (and retrying later).
pub const DEFAULT_MAX_POLLS: u32 = 900;

const PROVIDER: &str = "soniox";

/// The HTTP operations the adapter needs. Implemented by [`ReqwestTransport`]
/// for production and by fakes in tests.
#[allow(async_fn_in_trait)]
pub trait SonioxTransport {
    /// Upload an audio file, returning the provider file id.
    async fn upload_file(
        &self,
        audio: Vec<u8>,
        filename: &str,
        client_reference_id: &str,
    ) -> Result<String, SonioxError>;

    /// Create an async transcription, returning its provider id.
    async fn create_transcription(
        &self,
        request: &CreateTranscriptionRequest<'_>,
    ) -> Result<String, SonioxError>;

    /// Fetch a transcription's status document.
    async fn get_transcription(&self, transcription_id: &str) -> Result<Value, SonioxError>;

    /// Fetch a completed transcription's transcript document.
    async fn get_transcript(&self, transcription_id: &str) -> Result<Value, SonioxError>;

    /// Delete a transcription (best-effort cleanup).
    async fn delete_transcription(&self, transcription_id: &str) -> Result<(), SonioxError>;

    /// Delete an uploaded file (best-effort cleanup).
    async fn delete_file(&self, file_id: &str) -> Result<(), SonioxError>;
}

/// A reason the adapter could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum SonioxBuildError {
    /// The configured API key environment variable is not set.
    #[error("environment variable {0} is not set")]
    MissingApiKey(String),
    /// The configured API key environment variable is empty.
    #[error("environment variable {0} is empty")]
    BlankApiKey(String),
}

/// Soniox-backed [`Transcriber`].
pub struct SonioxTranscriber<T: SonioxTransport> {
    config: SonioxConfig,
    transport: T,
    poll_interval: Duration,
    max_polls: u32,
}

impl SonioxTranscriber<ReqwestTransport> {
    /// Build a production transcriber, reading the API key from the environment
    /// variable named by `config.api_key_env`.
    pub fn from_config(
        config: &SonioxConfig,
        http: reqwest::Client,
    ) -> Result<Self, SonioxBuildError> {
        let api_key = read_api_key(&config.api_key_env)?;
        let transport = ReqwestTransport::new(http, DEFAULT_BASE_URL.to_string(), api_key);
        Ok(Self::with_transport(
            config.clone(),
            transport,
            DEFAULT_POLL_INTERVAL,
            DEFAULT_MAX_POLLS,
        ))
    }
}

impl<T: SonioxTransport> SonioxTranscriber<T> {
    /// Build a transcriber over an explicit transport (used in tests).
    pub fn with_transport(
        config: SonioxConfig,
        transport: T,
        poll_interval: Duration,
        max_polls: u32,
    ) -> Self {
        Self {
            config,
            transport,
            poll_interval,
            max_polls,
        }
    }

    async fn run(
        &self,
        request: TranscriptionRequest<'_>,
    ) -> Result<TranscriptionOutput, WorkerFailure> {
        let recording_id = request.recording.id.clone();
        let audio = read_audio(&request.audio_path).await?;
        let filename = file_name(&request.audio_path);

        let file_id = self
            .transport
            .upload_file(audio, &filename, &recording_id)
            .await
            .map_err(SonioxError::into_worker_failure)?;

        let create = CreateTranscriptionRequest {
            model: &self.config.model,
            file_id: &file_id,
            language_hints: &self.config.language_hints,
            enable_speaker_diarization: self.config.enable_speaker_diarization,
            enable_language_identification: self.config.enable_language_identification,
            client_reference_id: &recording_id,
        };
        let transcription_id = match self.transport.create_transcription(&create).await {
            Ok(id) => id,
            Err(error) => {
                // Upload succeeded but create failed: clean up the orphan file.
                self.cleanup_file(&file_id).await;
                return Err(error.into_worker_failure());
            }
        };

        let status = match self.poll(&transcription_id).await {
            Ok(status) => status,
            Err(failure) => {
                self.cleanup(&transcription_id, &file_id).await;
                return Err(failure);
            }
        };

        let transcript = match self.transport.get_transcript(&transcription_id).await {
            Ok(transcript) => transcript,
            Err(error) => {
                self.cleanup(&transcription_id, &file_id).await;
                return Err(error.into_worker_failure());
            }
        };

        let text = render::render_transcript(&transcript);
        let raw_json = serde_json::json!({
            "transcription": status,
            "transcript": transcript,
        })
        .to_string();

        // Best-effort cleanup; a cleanup failure must not discard the Transcript.
        self.cleanup(&transcription_id, &file_id).await;

        Ok(TranscriptionOutput {
            provider: PROVIDER.to_string(),
            text,
            raw_json,
            provider_file_id: Some(file_id),
            provider_transcription_id: Some(transcription_id),
        })
    }

    /// Poll the transcription until it completes, errors, or the budget runs
    /// out. Returns the completed status document on success.
    async fn poll(&self, transcription_id: &str) -> Result<Value, WorkerFailure> {
        for _ in 0..self.max_polls {
            let status = self
                .transport
                .get_transcription(transcription_id)
                .await
                .map_err(SonioxError::into_worker_failure)?;

            match status.get("status").and_then(Value::as_str) {
                Some("completed") => return Ok(status),
                Some("error") => return Err(provider_status_error(&status)),
                Some("queued") | Some("processing") => {
                    sleep(self.poll_interval).await;
                }
                // An unknown status is treated as in-progress.
                Some(_) => sleep(self.poll_interval).await,
                None => {
                    return Err(WorkerFailure::Terminal {
                        code: "soniox_decode".to_string(),
                        message: "transcription response missing status".to_string(),
                    });
                }
            }
        }

        Err(WorkerFailure::Retryable {
            code: "soniox_poll_timeout".to_string(),
            message: format!("transcription not completed after {} polls", self.max_polls),
        })
    }

    async fn cleanup(&self, transcription_id: &str, file_id: &str) {
        self.cleanup_transcription(transcription_id).await;
        self.cleanup_file(file_id).await;
    }

    async fn cleanup_transcription(&self, transcription_id: &str) {
        if let Err(error) = self.transport.delete_transcription(transcription_id).await
            && !error.is_not_found()
        {
            eprintln!("soniox cleanup: delete transcription {transcription_id} failed: {error}");
        }
    }

    async fn cleanup_file(&self, file_id: &str) {
        if let Err(error) = self.transport.delete_file(file_id).await
            && !error.is_not_found()
        {
            eprintln!("soniox cleanup: delete file {file_id} failed: {error}");
        }
    }
}

impl<T: SonioxTransport> Transcriber for SonioxTranscriber<T> {
    async fn transcribe(
        &self,
        request: TranscriptionRequest<'_>,
    ) -> Result<TranscriptionOutput, WorkerFailure> {
        self.run(request).await
    }
}

fn read_api_key(env_name: &str) -> Result<String, SonioxBuildError> {
    let value = std::env::var(env_name)
        .map_err(|_| SonioxBuildError::MissingApiKey(env_name.to_string()))?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SonioxBuildError::BlankApiKey(env_name.to_string()));
    }
    Ok(trimmed.to_string())
}

async fn read_audio(path: &Path) -> Result<Vec<u8>, WorkerFailure> {
    tokio::fs::read(path)
        .await
        .map_err(|error| WorkerFailure::Terminal {
            code: "soniox_audio_unreadable".to_string(),
            message: format!("cannot read audio file: {error}"),
        })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("audio.wav")
        .to_string()
}

fn provider_status_error(status: &Value) -> WorkerFailure {
    let error_type = status.get("error_type").and_then(Value::as_str);
    let message = status
        .get("error_message")
        .and_then(Value::as_str)
        .or_else(|| status.get("message").and_then(Value::as_str));

    let code = match error_type {
        Some(kind) => format!("soniox_status_error_{kind}"),
        None => "soniox_status_error".to_string(),
    };
    let mut detail = "transcription failed".to_string();
    if let Some(kind) = error_type {
        detail.push_str(&format!("; error_type={kind}"));
    }
    if let Some(message) = message {
        detail.push_str(&format!("; message={message}"));
    }
    WorkerFailure::Terminal {
        code,
        message: detail,
    }
}

async fn sleep(duration: Duration) {
    if !duration.is_zero() {
        tokio::time::sleep(duration).await;
    }
}
