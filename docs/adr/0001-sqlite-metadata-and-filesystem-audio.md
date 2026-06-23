# SQLite Metadata and Filesystem Audio Storage

Telegraph Center stores Recording metadata and processing state in SQLite while storing uploaded audio as files in an app-owned data directory. This keeps the homelab deployment simple compared with Postgres, while avoiding a fragile filesystem-only state model once transcription, routing, backlog, and delivery retries need to be queried and updated independently.
