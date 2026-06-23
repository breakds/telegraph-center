# Secrets Come From Environment

Telegraph Center configuration may name environment variables for secrets, but must not place secret values such as the Soniox API key directly in config files. Deployment is expected to be managed by NixOS with ragenix, so secret material is injected into the service environment while static configuration remains non-sensitive and reviewable.
