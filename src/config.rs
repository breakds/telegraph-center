//! TOML configuration loading and validation.
//!
//! Configuration is non-sensitive and reviewable: it may name environment
//! variables that hold secrets, but it never contains secret values, and this
//! module never reads those environment variables. Parsing happens in two
//! stages: deserialize the raw TOML, then validate it into a typed
//! [`AppConfig`].

use std::path::PathBuf;

use serde::Deserialize;

use crate::domain::{TagError, Tags};

/// A fully validated application configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Data directory and intake limits.
    pub data: DataConfig,
    /// Configured Clients allowed to submit Recordings.
    pub clients: Vec<ClientConfig>,
    /// The single configured Operator account.
    pub operator: OperatorConfig,
    /// Soniox transcription settings.
    pub soniox: SonioxConfig,
    /// Configured Sinks.
    pub sinks: Vec<Sink>,
}

/// HTTP server settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    /// Address the server listens on.
    pub listen: String,
    /// Base path the service is mounted under, possibly empty.
    pub public_base_path: String,
}

/// Data directory and intake limits.
#[derive(Debug, Clone, PartialEq)]
pub struct DataConfig {
    /// App-owned directory for the database and audio blobs.
    pub dir: PathBuf,
    /// Maximum accepted upload size in bytes.
    pub max_upload_bytes: u64,
}

/// A configured Client identified by its certificate fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientConfig {
    /// Stable Client name used as the Client identity in storage.
    pub name: String,
    /// mTLS certificate fingerprint passed by nginx.
    pub certificate_fingerprint: String,
}

/// The single configured Operator account.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorConfig {
    /// Operator login name.
    pub username: String,
    /// Name of the environment variable holding the Argon2id password hash.
    pub password_hash_env: String,
}

/// Soniox transcription settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SonioxConfig {
    /// Name of the environment variable holding the Soniox API key.
    pub api_key_env: String,
    /// Soniox model identifier.
    pub model: String,
    /// Language hints passed to Soniox.
    pub language_hints: Vec<String>,
    /// Whether speaker diarization is enabled.
    pub enable_speaker_diarization: bool,
    /// Whether language identification is enabled.
    pub enable_language_identification: bool,
}

/// A configured destination for Transcript payloads.
#[derive(Debug, Clone, PartialEq)]
pub enum Sink {
    /// A Sink that delivers via an HTTP webhook.
    Webhook(WebhookSink),
}

/// A Sink that delivers a Transcript payload by HTTP webhook.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookSink {
    /// Unique Sink name.
    pub name: String,
    /// Fixed destination URL.
    pub url: String,
    /// Name of the environment variable holding the HMAC signing secret.
    pub secret_env: String,
    /// Tags that route a Recording to this Sink.
    pub match_tags: Tags,
}

impl Sink {
    /// The Sink's unique name.
    pub fn name(&self) -> &str {
        match self {
            Sink::Webhook(sink) => &sink.name,
        }
    }

    /// The tags that route a Recording to this Sink.
    pub fn match_tags(&self) -> &Tags {
        match self {
            Sink::Webhook(sink) => &sink.match_tags,
        }
    }
}

/// A rule that selects a Sink for a Recording by tag match.
///
/// V1 derives one rule per Sink from its `match_tags`. Rules are evaluated
/// top-to-bottom and the first match wins; no match means the Backlog.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingRule {
    /// Name of the Sink this rule selects.
    pub sink_name: String,
    /// Tags that trigger selection.
    pub match_tags: Tags,
}

impl RoutingRule {
    /// Whether a Recording with `recording_tags` matches this rule.
    pub fn matches(&self, recording_tags: &Tags) -> bool {
        self.match_tags.intersects(recording_tags)
    }
}

impl AppConfig {
    /// Parse and validate configuration from a TOML string.
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(input)?;
        raw.validate()
    }

    /// Find the configured Client whose certificate fingerprint matches.
    pub fn client_by_fingerprint(&self, fingerprint: &str) -> Option<&ClientConfig> {
        self.clients
            .iter()
            .find(|client| client.certificate_fingerprint == fingerprint)
    }

    /// The ordered Routing Rules derived from configured Sinks.
    pub fn routing_rules(&self) -> Vec<RoutingRule> {
        self.sinks
            .iter()
            .map(|sink| RoutingRule {
                sink_name: sink.name().to_string(),
                match_tags: sink.match_tags().clone(),
            })
            .collect()
    }
}

/// A reason configuration failed to load or validate.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The TOML could not be parsed.
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    /// A required text field was empty.
    #[error("{field} must not be empty")]
    Empty {
        /// The offending field.
        field: String,
    },
    /// Two Clients share a name.
    #[error("duplicate client name: {0:?}")]
    DuplicateClientName(String),
    /// Two Clients share a certificate fingerprint.
    #[error("duplicate client certificate fingerprint: {0:?}")]
    DuplicateClientFingerprint(String),
    /// Two Sinks share a name.
    #[error("duplicate sink name: {0:?}")]
    DuplicateSinkName(String),
    /// A Sink declared an unsupported type.
    #[error("unsupported sink type {kind:?} for sink {sink:?}")]
    UnsupportedSinkType {
        /// The Sink name.
        sink: String,
        /// The unsupported type value.
        kind: String,
    },
    /// A Sink URL was empty or not HTTP/HTTPS.
    #[error("invalid sink url {url:?} for sink {sink:?}")]
    InvalidSinkUrl {
        /// The Sink name.
        sink: String,
        /// The offending URL.
        url: String,
    },
    /// A Sink's `match_tags` failed tag validation.
    #[error("invalid match_tags for sink {sink:?}: {source}")]
    InvalidMatchTags {
        /// The Sink name.
        sink: String,
        /// The underlying tag error.
        source: TagError,
    },
}

// ---------------------------------------------------------------------------
// Raw deserialization layer
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    data: RawData,
    #[serde(default)]
    clients: Vec<RawClient>,
    operator: RawOperator,
    #[serde(default)]
    soniox: RawSoniox,
    #[serde(default)]
    sinks: Vec<RawSink>,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default)]
    public_base_path: String,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            public_base_path: String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawData {
    dir: PathBuf,
    #[serde(default = "default_max_upload_bytes")]
    max_upload_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct RawClient {
    name: String,
    certificate_fingerprint: String,
}

#[derive(Debug, Deserialize)]
struct RawOperator {
    username: String,
    password_hash_env: String,
}

#[derive(Debug, Deserialize)]
struct RawSoniox {
    #[serde(default)]
    api_key_env: String,
    #[serde(default = "default_soniox_model")]
    model: String,
    #[serde(default = "default_language_hints")]
    language_hints: Vec<String>,
    #[serde(default = "default_true")]
    enable_speaker_diarization: bool,
    #[serde(default)]
    enable_language_identification: bool,
}

impl Default for RawSoniox {
    fn default() -> Self {
        Self {
            api_key_env: String::new(),
            model: default_soniox_model(),
            language_hints: default_language_hints(),
            enable_speaker_diarization: true,
            enable_language_identification: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawSink {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    secret_env: String,
    #[serde(default)]
    match_tags: Vec<String>,
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_max_upload_bytes() -> u64 {
    268_435_456
}

fn default_soniox_model() -> String {
    "stt-async-v5".to_string()
}

fn default_language_hints() -> Vec<String> {
    vec!["en".to_string()]
}

fn default_true() -> bool {
    true
}

impl RawConfig {
    fn validate(self) -> Result<AppConfig, ConfigError> {
        let clients = validate_clients(self.clients)?;
        let operator = validate_operator(self.operator)?;
        let sinks = validate_sinks(self.sinks)?;

        Ok(AppConfig {
            server: ServerConfig {
                listen: self.server.listen,
                public_base_path: self.server.public_base_path,
            },
            data: DataConfig {
                dir: self.data.dir,
                max_upload_bytes: self.data.max_upload_bytes,
            },
            clients,
            operator,
            soniox: validate_soniox(self.soniox)?,
            sinks,
        })
    }
}

fn require_non_empty(value: String, field: &str) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::Empty {
            field: field.to_string(),
        })
    } else {
        Ok(value)
    }
}

fn validate_clients(raw: Vec<RawClient>) -> Result<Vec<ClientConfig>, ConfigError> {
    let mut clients = Vec::with_capacity(raw.len());
    for client in raw {
        let name = require_non_empty(client.name, "client name")?;
        let fingerprint = require_non_empty(
            client.certificate_fingerprint,
            "client certificate_fingerprint",
        )?;

        if clients.iter().any(|c: &ClientConfig| c.name == name) {
            return Err(ConfigError::DuplicateClientName(name));
        }
        if clients
            .iter()
            .any(|c: &ClientConfig| c.certificate_fingerprint == fingerprint)
        {
            return Err(ConfigError::DuplicateClientFingerprint(fingerprint));
        }

        clients.push(ClientConfig {
            name,
            certificate_fingerprint: fingerprint,
        });
    }
    Ok(clients)
}

fn validate_operator(raw: RawOperator) -> Result<OperatorConfig, ConfigError> {
    Ok(OperatorConfig {
        username: require_non_empty(raw.username, "operator username")?,
        password_hash_env: require_non_empty(raw.password_hash_env, "operator password_hash_env")?,
    })
}

fn validate_soniox(raw: RawSoniox) -> Result<SonioxConfig, ConfigError> {
    Ok(SonioxConfig {
        api_key_env: require_non_empty(raw.api_key_env, "soniox api_key_env")?,
        model: raw.model,
        language_hints: raw.language_hints,
        enable_speaker_diarization: raw.enable_speaker_diarization,
        enable_language_identification: raw.enable_language_identification,
    })
}

fn validate_sinks(raw: Vec<RawSink>) -> Result<Vec<Sink>, ConfigError> {
    let mut sinks: Vec<Sink> = Vec::with_capacity(raw.len());
    for sink in raw {
        let name = require_non_empty(sink.name, "sink name")?;

        if sinks.iter().any(|s| s.name() == name) {
            return Err(ConfigError::DuplicateSinkName(name));
        }

        if sink.kind != "webhook" {
            return Err(ConfigError::UnsupportedSinkType {
                sink: name,
                kind: sink.kind,
            });
        }

        if !is_http_url(&sink.url) {
            return Err(ConfigError::InvalidSinkUrl {
                sink: name,
                url: sink.url,
            });
        }

        let secret_env = require_non_empty(sink.secret_env, "sink secret_env")?;

        let match_tags =
            Tags::new(sink.match_tags).map_err(|source| ConfigError::InvalidMatchTags {
                sink: name.clone(),
                source,
            })?;

        sinks.push(Sink::Webhook(WebhookSink {
            name,
            url: sink.url,
            secret_env,
            match_tags,
        }));
    }
    Ok(sinks)
}

/// Conservative HTTP/HTTPS URL check.
///
/// TODO(M2+): replace with stricter parsing if a URL dependency is added.
fn is_http_url(url: &str) -> bool {
    (url.starts_with("http://") && url.len() > "http://".len())
        || (url.starts_with("https://") && url.len() > "https://".len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[server]
listen = "127.0.0.1:8080"
public_base_path = ""

[data]
dir = "/var/lib/telegraph-center"
max_upload_bytes = 268435456

[[clients]]
name = "litewatch-main"
certificate_fingerprint = "sha256:aaa"

[operator]
username = "break"
password_hash_env = "TELEGRAPH_OPERATOR_PASSWORD_HASH"

[soniox]
api_key_env = "SONIOX_API_KEY"
model = "stt-async-v5"
language_hints = ["en"]
enable_speaker_diarization = true
enable_language_identification = false

[[sinks]]
name = "journal"
type = "webhook"
url = "http://127.0.0.1:8644/webhooks/journal"
secret_env = "HERMES_JOURNAL_WEBHOOK_SECRET"
match_tags = ["Journal", "diary"]
"#;

    #[test]
    fn parses_valid_config() {
        let config = AppConfig::from_toml_str(VALID).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8080");
        assert_eq!(config.data.max_upload_bytes, 268_435_456);
        assert_eq!(config.clients.len(), 1);
        assert_eq!(config.operator.username, "break");
        assert_eq!(config.sinks.len(), 1);
        assert_eq!(config.sinks[0].name(), "journal");
    }

    #[test]
    fn normalizes_match_tags() {
        let config = AppConfig::from_toml_str(VALID).unwrap();
        assert_eq!(
            config.sinks[0].match_tags().as_slice(),
            &["journal".to_string(), "diary".to_string()]
        );
    }

    #[test]
    fn derives_routing_rules_from_sinks() {
        let config = AppConfig::from_toml_str(VALID).unwrap();
        let rules = config.routing_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].sink_name, "journal");
        let tags = Tags::new(["journal"]).unwrap();
        assert!(rules[0].matches(&tags));
    }

    #[test]
    fn applies_defaults_for_omitted_fields() {
        let minimal = r#"
[data]
dir = "/var/lib/telegraph-center"

[operator]
username = "break"
password_hash_env = "TELEGRAPH_OPERATOR_PASSWORD_HASH"

[soniox]
api_key_env = "SONIOX_API_KEY"
"#;
        let config = AppConfig::from_toml_str(minimal).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8080");
        assert_eq!(config.data.max_upload_bytes, 268_435_456);
        assert_eq!(config.soniox.model, "stt-async-v5");
        assert_eq!(config.soniox.language_hints, vec!["en".to_string()]);
        assert!(config.soniox.enable_speaker_diarization);
        assert!(!config.soniox.enable_language_identification);
        assert!(config.sinks.is_empty());
    }

    #[test]
    fn rejects_duplicate_client_names() {
        let input = VALID.replace(
            "certificate_fingerprint = \"sha256:aaa\"",
            "certificate_fingerprint = \"sha256:aaa\"\n\n[[clients]]\nname = \"litewatch-main\"\ncertificate_fingerprint = \"sha256:bbb\"",
        );
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::DuplicateClientName(name) if name == "litewatch-main"
        ));
    }

    #[test]
    fn rejects_duplicate_client_fingerprints() {
        let input = VALID.replace(
            "certificate_fingerprint = \"sha256:aaa\"",
            "certificate_fingerprint = \"sha256:aaa\"\n\n[[clients]]\nname = \"second\"\ncertificate_fingerprint = \"sha256:aaa\"",
        );
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::DuplicateClientFingerprint(fp) if fp == "sha256:aaa"
        ));
    }

    #[test]
    fn rejects_empty_fingerprint() {
        let input = VALID.replace("sha256:aaa", "");
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::Empty { .. }
        ));
    }

    #[test]
    fn rejects_duplicate_sink_names() {
        let input = VALID.replace(
            "match_tags = [\"Journal\", \"diary\"]",
            "match_tags = [\"journal\"]\n\n[[sinks]]\nname = \"journal\"\ntype = \"webhook\"\nurl = \"http://127.0.0.1:8644/x\"\nsecret_env = \"S\"\nmatch_tags = [\"todo\"]",
        );
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::DuplicateSinkName(name) if name == "journal"
        ));
    }

    #[test]
    fn rejects_unsupported_sink_type() {
        let input = VALID.replace("type = \"webhook\"", "type = \"kafka\"");
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::UnsupportedSinkType { kind, .. } if kind == "kafka"
        ));
    }

    #[test]
    fn rejects_non_http_sink_url() {
        let input = VALID.replace(
            "url = \"http://127.0.0.1:8644/webhooks/journal\"",
            "url = \"ftp://example.com\"",
        );
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::InvalidSinkUrl { .. }
        ));
    }

    #[test]
    fn rejects_invalid_match_tags() {
        let input = VALID.replace(
            "match_tags = [\"Journal\", \"diary\"]",
            "match_tags = [\"not a tag\"]",
        );
        assert!(matches!(
            AppConfig::from_toml_str(&input).unwrap_err(),
            ConfigError::InvalidMatchTags { .. }
        ));
    }

    #[test]
    fn rejects_secret_value_shaped_config_only_by_naming_env() {
        // The config names an env var; it never carries the secret itself.
        let config = AppConfig::from_toml_str(VALID).unwrap();
        assert_eq!(config.soniox.api_key_env, "SONIOX_API_KEY");
        assert_eq!(
            config.operator.password_hash_env,
            "TELEGRAPH_OPERATOR_PASSWORD_HASH"
        );
    }
}
