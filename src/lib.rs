//! Telegraph Center: receive audio Recordings from authenticated Clients,
//! transcribe them, and route the resulting Transcript to a configured Sink or
//! the Backlog.
//!
//! This crate is organized so the core state model (domain types, config, and
//! SQLite persistence) is testable without HTTP, transcription, or webhook
//! delivery. Later milestones add the HTTP API, workers, and monitor UI.

pub mod blob;
pub mod config;
pub mod domain;
pub mod http;
pub mod storage;
pub mod wav;
