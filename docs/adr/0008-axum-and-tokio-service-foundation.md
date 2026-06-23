# Axum and Tokio Service Foundation

Telegraph Center uses Axum on Tokio as the v1 Rust service foundation. This supports one async binary for upload APIs, monitor routes, middleware, and background workers while keeping handlers testable through Tower service abstractions.
