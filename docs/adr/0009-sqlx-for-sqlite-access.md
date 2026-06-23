# SQLx for SQLite Access

Telegraph Center uses SQLx for SQLite access with explicit SQL rather than an ORM. This matches the Axum/Tokio async service model, supports migrations and repository-level tests, and keeps the persisted state model visible without introducing ORM abstractions.
