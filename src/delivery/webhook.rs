//! The webhook HTTP transport seam and its Reqwest implementation.
//!
//! The transport sends a fully-formed request (URL, headers, raw body) and
//! returns the response status plus a small body excerpt, or a transport error
//! for network failures. Mapping status to success/retry/terminal is the
//! [`super::WebhookSinkClient`]'s job, so tests can use a fake transport.

/// Maximum number of response body bytes captured for error messages.
pub const MAX_BODY_EXCERPT: usize = 2048;

/// A fully-formed webhook request.
pub struct WebhookHttpRequest<'a> {
    /// Destination URL.
    pub url: &'a str,
    /// Header name/value pairs to send.
    pub headers: Vec<(&'static str, String)>,
    /// Raw request body bytes (the exact bytes that were signed).
    pub body: Vec<u8>,
}

/// A completed webhook HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookResponse {
    /// HTTP status code.
    pub status: u16,
    /// A short, capped excerpt of the response body for diagnostics.
    pub body_excerpt: String,
}

/// Sends webhook requests. Implemented by [`ReqwestWebhookTransport`] for
/// production and by fakes in tests.
#[allow(async_fn_in_trait)]
pub trait WebhookTransport {
    /// Send the request, returning the response, or an `Err` describing a
    /// network/transport failure (no usable HTTP response).
    async fn post(&self, request: WebhookHttpRequest<'_>) -> Result<WebhookResponse, String>;
}

/// Reqwest-backed webhook transport.
pub struct ReqwestWebhookTransport {
    http: reqwest::Client,
}

impl ReqwestWebhookTransport {
    /// Build a transport over a Reqwest client.
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl WebhookTransport for ReqwestWebhookTransport {
    async fn post(&self, request: WebhookHttpRequest<'_>) -> Result<WebhookResponse, String> {
        let mut builder = self.http.post(request.url).body(request.body);
        for (name, value) in &request.headers {
            builder = builder.header(*name, value);
        }

        let response = builder.send().await.map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body_excerpt = read_excerpt(response).await?;
        Ok(WebhookResponse {
            status,
            body_excerpt,
        })
    }
}

/// Read at most [`MAX_BODY_EXCERPT`] bytes of the response body, then stop, so a
/// large or unbounded response cannot exhaust memory. The rest of the body is
/// discarded when the response is dropped.
async fn read_excerpt(mut response: reqwest::Response) -> Result<String, String> {
    let mut buffer: Vec<u8> = Vec::new();
    while buffer.len() < MAX_BODY_EXCERPT {
        match response.chunk().await.map_err(|error| error.to_string())? {
            Some(chunk) => {
                let take = (MAX_BODY_EXCERPT - buffer.len()).min(chunk.len());
                buffer.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break; // reached the cap mid-chunk
                }
            }
            None => break,
        }
    }
    Ok(String::from_utf8_lossy(&buffer).to_string())
}
