# Server-Rendered Monitor With Askama

The monitoring webapp is server-rendered HTML using Askama templates rather than a single-page frontend. The v1 monitor needs login, list/detail pages, and operational forms, so compile-time Rust templates keep deployment to one binary and avoid a separate JavaScript build pipeline.
