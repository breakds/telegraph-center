//! Pure retry/backoff/deadline calculations for Transcription and Delivery.
//!
//! These functions contain no I/O so they can be unit-tested directly. Workers
//! call them to decide when a failed unit of work becomes due again and when a
//! retryable failure has outlived its deadline and must become terminal.

use time::{Duration, OffsetDateTime};

/// Backoff applied after the failure of attempt `attempt_number` (1-based).
///
/// Schedule: 1m, 5m, 15m, 1h, then 6h for attempt 5 and beyond.
pub fn backoff(attempt_number: i64) -> Duration {
    match attempt_number {
        ..=1 => Duration::minutes(1),
        2 => Duration::minutes(5),
        3 => Duration::minutes(15),
        4 => Duration::hours(1),
        _ => Duration::hours(6),
    }
}

/// When the next attempt becomes due, measured from when the latest attempt
/// *finished* (i.e. when it failed), so a slow attempt does not become
/// immediately retryable.
pub fn next_retry_at(
    last_attempt_finished_at: OffsetDateTime,
    attempt_number: i64,
) -> OffsetDateTime {
    last_attempt_finished_at + backoff(attempt_number)
}

/// The retry window for Transcription: `max(2h, 2 * audio_duration)`, capped at
/// 24h. A missing or non-positive duration uses the 2h floor.
pub fn transcription_window(audio_duration_ms: Option<i64>) -> Duration {
    let floor = Duration::hours(2);
    let cap = Duration::hours(24);
    let window = match audio_duration_ms {
        Some(ms) if ms > 0 => std::cmp::max(floor, Duration::milliseconds(ms.saturating_mul(2))),
        _ => floor,
    };
    std::cmp::min(window, cap)
}

/// The Transcription retry deadline, anchored on the first attempt's start time.
pub fn transcription_deadline(
    first_attempt_started_at: OffsetDateTime,
    audio_duration_ms: Option<i64>,
) -> OffsetDateTime {
    first_attempt_started_at + transcription_window(audio_duration_ms)
}

/// The Delivery retry window: a fixed 24 hours from Sink selection.
pub const DELIVERY_WINDOW: Duration = Duration::hours(24);

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-06-23 12:00:00 UTC);

    #[test]
    fn backoff_schedule() {
        assert_eq!(backoff(1), Duration::minutes(1));
        assert_eq!(backoff(2), Duration::minutes(5));
        assert_eq!(backoff(3), Duration::minutes(15));
        assert_eq!(backoff(4), Duration::hours(1));
        assert_eq!(backoff(5), Duration::hours(6));
        assert_eq!(backoff(99), Duration::hours(6));
    }

    #[test]
    fn next_retry_adds_backoff() {
        assert_eq!(next_retry_at(T0, 1), T0 + Duration::minutes(1));
        assert_eq!(next_retry_at(T0, 4), T0 + Duration::hours(1));
    }

    #[test]
    fn transcription_window_uses_two_hour_floor_when_duration_missing() {
        assert_eq!(transcription_window(None), Duration::hours(2));
        assert_eq!(transcription_window(Some(0)), Duration::hours(2));
        // 30 minutes of audio -> 2 * 30m = 1h, still below the 2h floor.
        assert_eq!(
            transcription_window(Some(30 * 60 * 1000)),
            Duration::hours(2)
        );
    }

    #[test]
    fn transcription_window_uses_twice_duration_for_long_audio() {
        // 3 hours of audio -> 6h window.
        let three_hours_ms = 3 * 60 * 60 * 1000;
        assert_eq!(
            transcription_window(Some(three_hours_ms)),
            Duration::hours(6)
        );
    }

    #[test]
    fn transcription_window_caps_at_24_hours() {
        // 20 hours of audio would be 40h, capped to 24h.
        let twenty_hours_ms = 20 * 60 * 60 * 1000;
        assert_eq!(
            transcription_window(Some(twenty_hours_ms)),
            Duration::hours(24)
        );
    }

    #[test]
    fn transcription_deadline_anchored_on_first_attempt() {
        assert_eq!(transcription_deadline(T0, None), T0 + Duration::hours(2));
    }
}
