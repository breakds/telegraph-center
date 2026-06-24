//! Soniox HTTP request/response types and the Reqwest-backed transport.

use serde::Serialize;
use serde_json::Value;

use super::SonioxTransport;
use super::error::SonioxError;

/// JSON body for `POST /v1/transcriptions`.
#[derive(Debug, Serialize)]
pub struct CreateTranscriptionRequest<'a> {
    /// Soniox model identifier.
    pub model: &'a str,
    /// The uploaded file's provider id.
    pub file_id: &'a str,
    /// Language hints.
    pub language_hints: &'a [String],
    /// Whether to enable speaker diarization.
    pub enable_speaker_diarization: bool,
    /// Whether to enable language identification.
    pub enable_language_identification: bool,
    /// The Telegraph Center Recording id, for provider-side correlation.
    pub client_reference_id: &'a str,
}

/// Reqwest-backed Soniox transport.
///
/// Holds the API key, which is sent only as a bearer token and never logged.
pub struct ReqwestTransport {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ReqwestTransport {
    /// Build a transport for `base_url` (e.g. `https://api.soniox.com/v1`).
    pub fn new(http: reqwest::Client, base_url: String, api_key: String) -> Self {
        Self {
            http,
            base_url,
            api_key,
        }
    }
}

impl SonioxTransport for ReqwestTransport {
    async fn upload_file(
        &self,
        audio: Vec<u8>,
        filename: &str,
        client_reference_id: &str,
    ) -> Result<String, SonioxError> {
        let part = reqwest::multipart::Part::bytes(audio).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("client_reference_id", client_reference_id.to_string());

        let response = self
            .http
            .post(format!("{}/files", self.base_url))
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(transport_error)?;

        let value = read_json(response).await?;
        extract_id(&value, "file id")
    }

    async fn create_transcription(
        &self,
        request: &CreateTranscriptionRequest<'_>,
    ) -> Result<String, SonioxError> {
        let response = self
            .http
            .post(format!("{}/transcriptions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await
            .map_err(transport_error)?;

        let value = read_json(response).await?;
        extract_id(&value, "transcription id")
    }

    async fn get_transcription(&self, transcription_id: &str) -> Result<Value, SonioxError> {
        let response = self
            .http
            .get(format!(
                "{}/transcriptions/{transcription_id}",
                self.base_url
            ))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(transport_error)?;
        read_json(response).await
    }

    async fn get_transcript(&self, transcription_id: &str) -> Result<Value, SonioxError> {
        let response = self
            .http
            .get(format!(
                "{}/transcriptions/{transcription_id}/transcript",
                self.base_url
            ))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(transport_error)?;
        read_json(response).await
    }

    async fn delete_transcription(&self, transcription_id: &str) -> Result<(), SonioxError> {
        let response = self
            .http
            .delete(format!(
                "{}/transcriptions/{transcription_id}",
                self.base_url
            ))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(transport_error)?;
        check_status(response).await
    }

    async fn delete_file(&self, file_id: &str) -> Result<(), SonioxError> {
        let response = self
            .http
            .delete(format!("{}/files/{file_id}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(transport_error)?;
        check_status(response).await
    }
}

fn transport_error(error: reqwest::Error) -> SonioxError {
    SonioxError::Transport(error.to_string())
}

fn extract_id(value: &Value, what: &str) -> Result<String, SonioxError> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| SonioxError::Decode(format!("response missing {what}")))
}

/// Read a JSON body, turning a non-success status into a structured HTTP error.
async fn read_json(response: reqwest::Response) -> Result<Value, SonioxError> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(transport_error)?;
    if status.is_success() {
        serde_json::from_slice(&bytes).map_err(|error| SonioxError::Decode(error.to_string()))
    } else {
        Err(http_error(status.as_u16(), &bytes))
    }
}

/// Check a response status for endpoints with no useful body (DELETE).
async fn check_status(response: reqwest::Response) -> Result<(), SonioxError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let bytes = response.bytes().await.map_err(transport_error)?;
    Err(http_error(status.as_u16(), &bytes))
}

/// Build an HTTP error, extracting provider error fields if the body is JSON.
fn http_error(status: u16, bytes: &[u8]) -> SonioxError {
    let body: Option<Value> = serde_json::from_slice(bytes).ok();
    let (error_type, message, request_id) = body
        .as_ref()
        .map(extract_error_fields)
        .unwrap_or((None, None, None));
    SonioxError::Http {
        status,
        error_type,
        message,
        request_id,
    }
}

/// Pull `error_type` / `message` / `request_id` from a Soniox error body,
/// tolerating either a flat object or one nested under `error`.
fn extract_error_fields(body: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let source = body.get("error").unwrap_or(body);
    let field = |name: &str| source.get(name).and_then(Value::as_str).map(str::to_string);
    let error_type = field("error_type").or_else(|| field("type"));
    let message = field("message").or_else(|| field("error_message"));
    let request_id = field("request_id").or_else(|| field("requestId"));
    (error_type, message, request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_flat_error_fields() {
        let body = json!({
            "error_type": "invalid_request",
            "message": "bad audio",
            "request_id": "req-1"
        });
        let (error_type, message, request_id) = extract_error_fields(&body);
        assert_eq!(error_type.as_deref(), Some("invalid_request"));
        assert_eq!(message.as_deref(), Some("bad audio"));
        assert_eq!(request_id.as_deref(), Some("req-1"));
    }

    #[test]
    fn extracts_nested_error_fields() {
        let body = json!({ "error": { "type": "unauthenticated", "message": "no key" } });
        let (error_type, message, request_id) = extract_error_fields(&body);
        assert_eq!(error_type.as_deref(), Some("unauthenticated"));
        assert_eq!(message.as_deref(), Some("no key"));
        assert_eq!(request_id, None);
    }
}
