# App-Managed Operator Authentication

Telegraph Center handles Operator login for the monitoring webapp instead of relying only on nginx Basic Auth. The monitor may be accessed from a phone outside the homelab, so the app owns password verification, Operator Sessions, CSRF protection for state-changing actions, login throttling, and audit logging while nginx remains responsible for TLS and reverse-proxy exposure.

The v1 scope is one configured Operator account: no self-registration, password reset flow, or role system. The Operator password is verified against an Argon2id hash supplied through the deployment secret mechanism, not stored as plaintext in the static config.
