# Reqwest for Outbound HTTP Integrations

Telegraph Center uses Reqwest for outbound HTTP integrations, including Soniox transcription requests and Webhook Sink delivery. Direct HTTP clients behind small traits keep the integrations testable and avoid binding the core service to provider-specific SDKs.
