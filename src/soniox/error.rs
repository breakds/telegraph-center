//! Soniox transport/provider errors and their mapping to worker failures.

use crate::workers::WorkerFailure;

/// An error from the Soniox transport.
///
/// This never carries the API key. HTTP errors capture the status and any
/// provider-supplied `error_type` / `message` / `request_id` so failures are
/// debuggable from stored `latest_error` / attempt rows.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SonioxError {
    /// A network/connect/timeout error before a usable HTTP response.
    #[error("soniox transport error: {0}")]
    Transport(String),
    /// A successful HTTP response whose body could not be interpreted.
    #[error("soniox response decode error: {0}")]
    Decode(String),
    /// A non-success HTTP response.
    #[error("soniox http {status} error")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Provider `error_type`, if present.
        error_type: Option<String>,
        /// Provider message, if present.
        message: Option<String>,
        /// Provider `request_id`, if present.
        request_id: Option<String>,
    },
}

impl SonioxError {
    /// Whether this is an HTTP 404, used to ignore missing resources during
    /// best-effort cleanup.
    pub fn is_not_found(&self) -> bool {
        matches!(self, SonioxError::Http { status: 404, .. })
    }

    /// Map this error into a worker failure.
    ///
    /// Transport errors and 5xx/409 are retryable; decode errors and most 4xx
    /// are terminal. A 429 is retryable when it looks like transient rate or
    /// capacity pressure, but terminal when it names a persistent cap (total
    /// file count/size or total/pending transcription quota), since retrying the
    /// same Recording cannot clear those.
    pub fn into_worker_failure(self) -> WorkerFailure {
        match self {
            SonioxError::Transport(message) => WorkerFailure::Retryable {
                code: "soniox_transport".to_string(),
                message,
            },
            SonioxError::Decode(message) => WorkerFailure::Terminal {
                code: "soniox_decode".to_string(),
                message,
            },
            SonioxError::Http {
                status,
                error_type,
                message,
                request_id,
            } => {
                let code = match &error_type {
                    Some(kind) => format!("soniox_http_{status}_{kind}"),
                    None => format!("soniox_http_{status}"),
                };
                let detail = http_detail(
                    status,
                    error_type.as_deref(),
                    message.as_deref(),
                    request_id.as_deref(),
                );
                if is_retryable_http(status, error_type.as_deref(), message.as_deref()) {
                    WorkerFailure::Retryable {
                        code,
                        message: detail,
                    }
                } else {
                    WorkerFailure::Terminal {
                        code,
                        message: detail,
                    }
                }
            }
        }
    }
}

/// Whether a non-success HTTP response should be retried.
fn is_retryable_http(status: u16, error_type: Option<&str>, message: Option<&str>) -> bool {
    match status {
        // A 429 is retryable unless it names a persistent quota/cap.
        429 => !is_persistent_limit(error_type, message),
        s if s >= 500 => true,
        409 => true,
        _ => false,
    }
}

/// Whether a 429 describes a persistent cap (total file count/size, total or
/// pending transcription quota) rather than transient rate pressure.
///
/// Soniox returns 429 for both. Rate-limit messages talk about requests per
/// minute; cap messages talk about totals, pending work, storage, or quota.
fn is_persistent_limit(error_type: Option<&str>, message: Option<&str>) -> bool {
    const CAP_MARKERS: [&str; 4] = ["total", "pending", "quota", "storage"];
    let haystack = format!(
        "{} {}",
        error_type.unwrap_or_default(),
        message.unwrap_or_default()
    )
    .to_lowercase();
    CAP_MARKERS.iter().any(|marker| haystack.contains(marker))
}

/// Build a human-readable detail string for an HTTP error without leaking
/// secrets.
fn http_detail(
    status: u16,
    error_type: Option<&str>,
    message: Option<&str>,
    request_id: Option<&str>,
) -> String {
    let mut detail = format!("HTTP {status}");
    if let Some(kind) = error_type {
        detail.push_str(&format!("; error_type={kind}"));
    }
    if let Some(message) = message {
        detail.push_str(&format!("; message={message}"));
    }
    if let Some(request_id) = request_id {
        detail.push_str(&format!("; request_id={request_id}"));
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_is_retryable() {
        let failure = SonioxError::Transport("connection reset".into()).into_worker_failure();
        assert!(matches!(failure, WorkerFailure::Retryable { .. }));
    }

    #[test]
    fn server_errors_are_retryable() {
        for status in [500, 502, 503, 429, 409] {
            let failure = SonioxError::Http {
                status,
                error_type: None,
                message: None,
                request_id: None,
            }
            .into_worker_failure();
            assert!(
                matches!(failure, WorkerFailure::Retryable { .. }),
                "status {status} should be retryable"
            );
        }
    }

    #[test]
    fn client_errors_are_terminal() {
        for status in [400, 401, 402, 404] {
            let failure = SonioxError::Http {
                status,
                error_type: None,
                message: None,
                request_id: None,
            }
            .into_worker_failure();
            assert!(
                matches!(failure, WorkerFailure::Terminal { .. }),
                "status {status} should be terminal"
            );
        }
    }

    fn http_status(status: u16, error_type: &str, message: &str) -> WorkerFailure {
        SonioxError::Http {
            status,
            error_type: Some(error_type.to_string()),
            message: Some(message.to_string()),
            request_id: None,
        }
        .into_worker_failure()
    }

    #[test]
    fn rate_limit_429_is_retryable() {
        let failure = http_status(
            429,
            "limit_exceeded",
            "rate limit exceeded: too many requests per minute",
        );
        assert!(matches!(failure, WorkerFailure::Retryable { .. }));
    }

    #[test]
    fn quota_cap_429_is_terminal() {
        // Total file size / file count caps and total/pending transcription caps.
        for message in [
            "total file size limit exceeded",
            "total file count exceeded",
            "too many pending transcriptions",
            "account storage quota exceeded",
        ] {
            let failure = http_status(429, "limit_exceeded", message);
            assert!(
                matches!(failure, WorkerFailure::Terminal { .. }),
                "message {message:?} should be terminal"
            );
        }
    }

    #[test]
    fn bare_429_without_context_is_retryable() {
        let failure = SonioxError::Http {
            status: 429,
            error_type: None,
            message: None,
            request_id: None,
        }
        .into_worker_failure();
        assert!(matches!(failure, WorkerFailure::Retryable { .. }));
    }

    #[test]
    fn detail_includes_provider_fields_but_not_secrets() {
        let failure = SonioxError::Http {
            status: 401,
            error_type: Some("unauthenticated".into()),
            message: Some("invalid api key".into()),
            request_id: Some("req-123".into()),
        }
        .into_worker_failure();
        let WorkerFailure::Terminal { code, message } = failure else {
            panic!("expected terminal");
        };
        assert_eq!(code, "soniox_http_401_unauthenticated");
        assert!(message.contains("HTTP 401"));
        assert!(message.contains("error_type=unauthenticated"));
        assert!(message.contains("request_id=req-123"));
    }
}
