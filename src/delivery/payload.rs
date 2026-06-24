//! Deterministic Webhook Sink payload construction.
//!
//! The payload carries Recording metadata and the rendered Transcript text
//! only. It never includes audio, raw provider JSON, or provider IDs.

use serde::Serialize;

use crate::domain::{Delivery, Recording, Transcript, timestamp};

/// The JSON payload POSTed to a Webhook Sink.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WebhookPayload {
    /// Event type discriminator.
    pub event: &'static str,
    /// Server Recording id.
    pub recording_id: String,
    /// Stable Delivery id (also sent in headers for downstream dedup).
    pub delivery_id: String,
    /// Configured Client name.
    pub client_id: String,
    /// Client-assigned Recording id.
    pub client_recording_id: String,
    /// Optional Client capture time (RFC3339), `null` when absent.
    pub recorded_at: Option<String>,
    /// Server receive time (RFC3339), always present.
    pub received_at: String,
    /// Normalized Recording tags.
    pub tags: Vec<String>,
    /// The Transcript payload.
    pub transcript: TranscriptPayload,
}

/// The transcript portion of the payload.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TranscriptPayload {
    /// Rendered plain-text Transcript.
    pub text: String,
}

const EVENT: &str = "recording.transcribed";

/// Build the payload for a Recording's Delivery.
pub fn build(
    recording: &Recording,
    delivery: &Delivery,
    transcript: &Transcript,
) -> WebhookPayload {
    WebhookPayload {
        event: EVENT,
        recording_id: recording.id.clone(),
        delivery_id: delivery.id.clone(),
        client_id: recording.client_id.clone(),
        client_recording_id: recording.client_recording_id.clone(),
        recorded_at: recording.recorded_at.map(timestamp::format),
        received_at: timestamp::format(recording.received_at),
        tags: recording.tags.as_slice().to_vec(),
        transcript: TranscriptPayload {
            text: transcript.text.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeliveryStatus, RecordingStatus, Tags};
    use serde_json::Value;
    use time::OffsetDateTime;
    use time::macros::datetime;

    const RECORDED: OffsetDateTime = datetime!(2026-06-09 14:22:33 UTC);
    const RECEIVED: OffsetDateTime = datetime!(2026-06-09 14:23:10 UTC);

    fn recording(recorded_at: Option<OffsetDateTime>) -> Recording {
        Recording {
            id: "rec-1".to_string(),
            client_id: "litewatch-main".to_string(),
            client_recording_id: "20260609-142233".to_string(),
            status: RecordingStatus::Delivering,
            original_filename: Some("20260609-142233.wav".to_string()),
            blob_path: Some("recordings/rec-1.wav".to_string()),
            audio_size_bytes: Some(1000),
            audio_duration_ms: Some(1000),
            sample_rate_hz: Some(16000),
            channels: Some(1),
            bits_per_sample: Some(16),
            tags: Tags::new(["journal"]).unwrap(),
            recorded_at,
            received_at: RECEIVED,
            selected_sink_name: Some("journal".to_string()),
            latest_error: None,
            created_at: RECEIVED,
            updated_at: RECEIVED,
        }
    }

    fn delivery() -> Delivery {
        Delivery {
            id: "del-1".to_string(),
            recording_id: "rec-1".to_string(),
            sink_name: "journal".to_string(),
            status: DeliveryStatus::Delivering,
            selected_at: RECEIVED,
            completed_at: None,
            retry_deadline_at: None,
            latest_error: None,
        }
    }

    fn transcript() -> Transcript {
        Transcript {
            recording_id: "rec-1".to_string(),
            provider: "soniox".to_string(),
            text: "hello world".to_string(),
            raw_json: r#"{"secret":"do not send"}"#.to_string(),
            provider_file_id: Some("file-1".to_string()),
            provider_transcription_id: Some("tr-1".to_string()),
            created_at: RECEIVED,
        }
    }

    #[test]
    fn payload_matches_contract() {
        let payload = build(&recording(Some(RECORDED)), &delivery(), &transcript());
        let json: Value = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["event"], "recording.transcribed");
        assert_eq!(json["recording_id"], "rec-1");
        assert_eq!(json["delivery_id"], "del-1");
        assert_eq!(json["client_id"], "litewatch-main");
        assert_eq!(json["client_recording_id"], "20260609-142233");
        assert_eq!(json["recorded_at"], "2026-06-09T14:22:33Z");
        assert_eq!(json["received_at"], "2026-06-09T14:23:10Z");
        assert_eq!(json["tags"], serde_json::json!(["journal"]));
        assert_eq!(json["transcript"]["text"], "hello world");
    }

    #[test]
    fn recorded_at_is_null_when_absent() {
        let payload = build(&recording(None), &delivery(), &transcript());
        let json: Value = serde_json::to_value(&payload).unwrap();
        assert!(json.get("recorded_at").is_some(), "field is present");
        assert!(json["recorded_at"].is_null(), "and null");
    }

    #[test]
    fn omits_audio_and_provider_internals() {
        let payload = build(&recording(Some(RECORDED)), &delivery(), &transcript());
        let body = serde_json::to_string(&payload).unwrap();
        assert!(!body.contains("raw_json"));
        assert!(!body.contains("do not send"));
        assert!(!body.contains("blob_path"));
        assert!(!body.contains("recordings/"));
        assert!(!body.contains("provider_file_id"));
        assert!(!body.contains("file-1"));
    }
}
