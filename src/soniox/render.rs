//! Pure rendering of a Soniox transcript response into plain text.
//!
//! When tokens carry speaker labels, consecutive tokens are grouped by speaker
//! and each speaker turn is prefixed with `Speaker <id>:`. Otherwise the
//! provider's own `text` field is used.

use serde::Deserialize;
use serde_json::Value;

/// A single transcript token, as returned under `tokens`.
#[derive(Debug, Deserialize)]
pub struct Token {
    /// The token text, including any leading whitespace from the provider.
    #[serde(default)]
    pub text: String,
    /// The speaker label, if speaker diarization is enabled.
    #[serde(default)]
    pub speaker: Option<Speaker>,
}

/// A speaker identifier, which Soniox may return as a number or a string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Speaker {
    /// Numeric speaker id.
    Int(i64),
    /// String speaker id.
    Str(String),
}

impl std::fmt::Display for Speaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Speaker::Int(value) => write!(f, "{value}"),
            Speaker::Str(value) => f.write_str(value),
        }
    }
}

/// Render the transcript portion of a Soniox transcript response into text.
pub fn render_transcript(transcript: &Value) -> String {
    let text = transcript
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tokens: Vec<Token> = transcript
        .get("tokens")
        .and_then(|tokens| serde_json::from_value(tokens.clone()).ok())
        .unwrap_or_default();
    render(text, &tokens)
}

/// Render text from a fallback `text` and parsed `tokens`.
pub fn render(text: &str, tokens: &[Token]) -> String {
    let has_speakers = tokens.iter().any(|token| token.speaker.is_some());
    if !has_speakers {
        return text.trim().to_string();
    }

    let mut out = String::new();
    let mut current: Option<String> = None;
    let mut segment = String::new();

    for token in tokens {
        let speaker = token.speaker.as_ref().map(Speaker::to_string);
        if speaker != current {
            push_segment(&mut out, &current, &segment);
            segment.clear();
            current = speaker;
        }
        segment.push_str(&token.text);
    }
    push_segment(&mut out, &current, &segment);
    out
}

fn push_segment(out: &mut String, speaker: &Option<String>, segment: &str) {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    match speaker {
        Some(id) => out.push_str(&format!("Speaker {id}: {trimmed}")),
        None => out.push_str(trimmed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_text_when_no_speakers() {
        let transcript = json!({
            "text": "hello world",
            "tokens": [{ "text": "hello" }, { "text": " world" }]
        });
        assert_eq!(render_transcript(&transcript), "hello world");
    }

    #[test]
    fn falls_back_to_text_with_no_tokens() {
        let transcript = json!({ "text": "  just text  " });
        assert_eq!(render_transcript(&transcript), "just text");
    }

    #[test]
    fn groups_consecutive_tokens_by_speaker() {
        let transcript = json!({
            "text": "Hi there how are you",
            "tokens": [
                { "text": "Hi", "speaker": 1 },
                { "text": " there", "speaker": 1 },
                { "text": "how", "speaker": 2 },
                { "text": " are you", "speaker": 2 },
                { "text": "fine", "speaker": 1 }
            ]
        });
        assert_eq!(
            render_transcript(&transcript),
            "Speaker 1: Hi there\nSpeaker 2: how are you\nSpeaker 1: fine"
        );
    }

    #[test]
    fn supports_string_speaker_ids() {
        let transcript = json!({
            "text": "hello",
            "tokens": [{ "text": "hello", "speaker": "alice" }]
        });
        assert_eq!(render_transcript(&transcript), "Speaker alice: hello");
    }
}
