//! HMAC-signed Webhook Sink Delivery.
//!
//! [`WebhookSinkClient`] implements [`crate::workers::SinkClient`]. It builds the
//! JSON [`payload`], signs the exact serialized bytes with the Sink's secret
//! ([`hmac`]), and POSTs them through a [`webhook::WebhookTransport`] with the
//! signature and stable Delivery id headers. Secrets are read from the
//! environment at construction and never logged or placed in config.

pub mod hmac;
pub mod payload;
pub mod webhook;

use crate::config::Sink;
use crate::workers::{DeliveryRequest, SinkClient, WorkerFailure};

use webhook::{WebhookHttpRequest, WebhookResponse, WebhookTransport};

const HEADER_SIGNATURE: &str = "X-Webhook-Signature";
const HEADER_REQUEST_ID: &str = "X-Request-ID";
const HEADER_DELIVERY_ID: &str = "X-Telegraph-Delivery-Id";

/// A configured Webhook Sink with its secret resolved from the environment.
pub struct ResolvedSink {
    name: String,
    url: String,
    secret: Vec<u8>,
}

impl ResolvedSink {
    /// Build a resolved Sink directly (used in tests).
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        secret: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            secret: secret.into(),
        }
    }
}

/// A reason the Webhook Sink client could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum WebhookBuildError {
    /// A Sink's secret environment variable is not set.
    #[error("environment variable {0} is not set")]
    MissingSecret(String),
    /// A Sink's secret environment variable is empty.
    #[error("environment variable {0} is empty")]
    BlankSecret(String),
}

/// A [`SinkClient`] that delivers to configured Webhook Sinks.
pub struct WebhookSinkClient<T: WebhookTransport> {
    sinks: Vec<ResolvedSink>,
    transport: T,
}

impl WebhookSinkClient<webhook::ReqwestWebhookTransport> {
    /// Build a production client, reading each Webhook Sink's secret from its
    /// configured `secret_env`.
    pub fn from_config(sinks: &[Sink], http: reqwest::Client) -> Result<Self, WebhookBuildError> {
        let resolved = resolve_sinks(sinks)?;
        Ok(Self::with_transport(
            resolved,
            webhook::ReqwestWebhookTransport::new(http),
        ))
    }
}

impl<T: WebhookTransport> WebhookSinkClient<T> {
    /// Build a client over explicit resolved Sinks and a transport (tests).
    pub fn with_transport(sinks: Vec<ResolvedSink>, transport: T) -> Self {
        Self { sinks, transport }
    }

    fn find(&self, name: &str) -> Option<&ResolvedSink> {
        self.sinks.iter().find(|sink| sink.name == name)
    }
}

impl<T: WebhookTransport> SinkClient for WebhookSinkClient<T> {
    async fn deliver(&self, request: DeliveryRequest<'_>) -> Result<(), WorkerFailure> {
        let Some(sink) = self.find(&request.delivery.sink_name) else {
            return Err(WorkerFailure::Terminal {
                code: "unknown_sink".to_string(),
                message: format!("no configured sink named {:?}", request.delivery.sink_name),
            });
        };

        let payload = payload::build(request.recording, request.delivery, request.transcript);
        let body = serde_json::to_vec(&payload).map_err(|error| WorkerFailure::Terminal {
            code: "payload_serialize".to_string(),
            message: error.to_string(),
        })?;
        let signature = hmac::sign(&sink.secret, &body);

        let headers = vec![
            ("Content-Type", "application/json".to_string()),
            (HEADER_SIGNATURE, signature),
            (HEADER_REQUEST_ID, request.delivery.id.clone()),
            (HEADER_DELIVERY_ID, request.delivery.id.clone()),
        ];

        let http_request = WebhookHttpRequest {
            url: &sink.url,
            headers,
            body,
        };

        match self.transport.post(http_request).await {
            Ok(response) => map_response(response),
            Err(error) => Err(WorkerFailure::Retryable {
                code: "webhook_transport".to_string(),
                message: error,
            }),
        }
    }
}

fn resolve_sinks(sinks: &[Sink]) -> Result<Vec<ResolvedSink>, WebhookBuildError> {
    sinks
        .iter()
        .map(|sink| match sink {
            Sink::Webhook(webhook) => Ok(ResolvedSink {
                name: webhook.name.clone(),
                url: webhook.url.clone(),
                secret: read_secret(&webhook.secret_env)?,
            }),
        })
        .collect()
}

fn read_secret(env_name: &str) -> Result<Vec<u8>, WebhookBuildError> {
    let value = std::env::var(env_name)
        .map_err(|_| WebhookBuildError::MissingSecret(env_name.to_string()))?;
    if value.trim().is_empty() {
        return Err(WebhookBuildError::BlankSecret(env_name.to_string()));
    }
    Ok(value.into_bytes())
}

/// Map a completed webhook response to worker success or failure.
///
/// 2xx is success; 5xx and 429 are retryable; every other status (including
/// 3xx and non-429 4xx) is terminal.
fn map_response(response: WebhookResponse) -> Result<(), WorkerFailure> {
    let status = response.status;
    if (200..300).contains(&status) {
        return Ok(());
    }

    let excerpt = response.body_excerpt.trim();
    let message = if excerpt.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {excerpt}")
    };
    let code = format!("webhook_http_{status}");

    if status >= 500 || status == 429 {
        Err(WorkerFailure::Retryable { code, message })
    } else {
        Err(WorkerFailure::Terminal { code, message })
    }
}
