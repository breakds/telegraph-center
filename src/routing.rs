//! Config-driven Routing Rule evaluation.
//!
//! A [`ConfigRouter`] holds the Routing Rules derived from configured Sinks (one
//! per Sink, in TOML order). It selects the first Sink whose `match_tags`
//! intersect the Recording's tags, and otherwise sends the Recording to the
//! Backlog. There is no default Sink.

use crate::config::{AppConfig, RoutingRule};
use crate::domain::Recording;
use crate::workers::{Router, RoutingDecision};

/// A [`Router`] backed by configured Routing Rules.
pub struct ConfigRouter {
    rules: Vec<RoutingRule>,
}

impl ConfigRouter {
    /// Build a router from an explicit ordered rule list.
    pub fn new(rules: Vec<RoutingRule>) -> Self {
        Self { rules }
    }

    /// Build a router from the application configuration.
    pub fn from_config(config: &AppConfig) -> Self {
        Self::new(config.routing_rules())
    }
}

impl Router for ConfigRouter {
    fn route(&self, recording: &Recording) -> RoutingDecision {
        for rule in &self.rules {
            if rule.matches(&recording.tags) {
                return RoutingDecision::Sink(rule.sink_name.clone());
            }
        }
        RoutingDecision::Backlog
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::domain::{RecordingStatus, Tags};
    use time::OffsetDateTime;
    use time::macros::datetime;

    const WHEN: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);

    fn recording_with_tags(tags: &[&str]) -> Recording {
        Recording {
            id: "rec-1".to_string(),
            client_id: "litewatch-main".to_string(),
            client_recording_id: "cr-1".to_string(),
            status: RecordingStatus::Routing,
            original_filename: None,
            blob_path: None,
            audio_size_bytes: None,
            audio_duration_ms: None,
            sample_rate_hz: None,
            channels: None,
            bits_per_sample: None,
            tags: Tags::new(tags.iter().copied()).unwrap(),
            recorded_at: None,
            received_at: WHEN,
            selected_sink_name: None,
            latest_error: None,
            created_at: WHEN,
            updated_at: WHEN,
        }
    }

    fn router() -> ConfigRouter {
        let toml = r#"
[data]
dir = "/var/lib/telegraph-center"

[operator]
username = "break"
password_hash_env = "TELEGRAPH_OPERATOR_PASSWORD_HASH"

[soniox]
api_key_env = "SONIOX_API_KEY"

[[sinks]]
name = "journal"
type = "webhook"
url = "http://127.0.0.1:8644/webhooks/journal"
secret_env = "JOURNAL_SECRET"
match_tags = ["journal", "diary"]

[[sinks]]
name = "todo"
type = "webhook"
url = "http://127.0.0.1:8644/webhooks/todo"
secret_env = "TODO_SECRET"
match_tags = ["todo"]
"#;
        ConfigRouter::from_config(&AppConfig::from_toml_str(toml).unwrap())
    }

    fn sink_name(decision: RoutingDecision) -> Option<String> {
        match decision {
            RoutingDecision::Sink(name) => Some(name),
            RoutingDecision::Backlog => None,
        }
    }

    #[test]
    fn first_matching_sink_wins() {
        // A Recording tagged with both should match the earlier Sink (journal).
        let decision = router().route(&recording_with_tags(&["todo", "journal"]));
        assert_eq!(sink_name(decision).as_deref(), Some("journal"));
    }

    #[test]
    fn matches_second_sink_when_first_does_not() {
        let decision = router().route(&recording_with_tags(&["todo"]));
        assert_eq!(sink_name(decision).as_deref(), Some("todo"));
    }

    #[test]
    fn no_matching_tag_is_backlog() {
        let decision = router().route(&recording_with_tags(&["unrelated"]));
        assert!(matches!(decision, RoutingDecision::Backlog));
    }

    #[test]
    fn empty_recording_tags_are_backlog() {
        let decision = router().route(&recording_with_tags(&[]));
        assert!(matches!(decision, RoutingDecision::Backlog));
    }

    #[test]
    fn empty_match_tags_never_match() {
        // A Sink with empty match_tags should not match a tagged Recording.
        let router = ConfigRouter::new(vec![RoutingRule {
            sink_name: "catch-all".to_string(),
            match_tags: Tags::default(),
        }]);
        let decision = router.route(&recording_with_tags(&["journal"]));
        assert!(matches!(decision, RoutingDecision::Backlog));
    }
}
