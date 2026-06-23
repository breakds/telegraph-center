# HMAC-Signed Webhook Sinks

Webhook Sink delivery signs the raw JSON request body with HMAC-SHA256 and sends the hex digest in `X-Webhook-Signature`, matching Hermes's generic webhook validation. The same request includes `X-Request-ID` set to Telegraph Center's stable Delivery ID so Hermes can deduplicate retries; signing secrets are read through environment-backed secret configuration.
