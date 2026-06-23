# Client Identity From Nginx mTLS Fingerprint

Telegraph Center relies on nginx to validate Client mTLS certificates and pass a certificate fingerprint to the app, where it is mapped to a configured Client. This keeps certificate handling at the TLS edge while preserving app-level authorization and idempotency keyed by Client; nginx must clear inbound identity headers before setting the trusted fingerprint header.
