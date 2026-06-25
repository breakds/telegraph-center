-- Manual Retry support: when an Operator retries failed Transcription or
-- Delivery, this records the retry time. The worker treats the work as
-- immediately due (bypassing the prior attempt's backoff) until an attempt runs
-- within the window, and Transcription measures its retry deadline from here so
-- a manual retry gets a fresh window rather than one inherited expired attempt.

ALTER TABLE recordings ADD COLUMN retry_window_started_at TEXT;
ALTER TABLE deliveries ADD COLUMN retry_window_started_at TEXT;
