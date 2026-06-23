//! Testability seams shared across the service.
//!
//! Wall-clock time and identifier generation are injected through small traits
//! so HTTP handlers and background workers can be driven deterministically in
//! tests while production uses the system clock and ULIDs.

use time::OffsetDateTime;

/// Source of wall-clock time.
pub trait Clock: Send + Sync {
    /// The current time in UTC.
    fn now(&self) -> OffsetDateTime;
}

/// Production clock backed by the system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// Source of server-generated identifiers (Recordings, attempts, Deliveries).
pub trait IdGenerator: Send + Sync {
    /// A fresh, stable identifier.
    fn generate(&self) -> String;
}

/// Production identifier generator producing lexicographically sortable ULIDs.
#[derive(Debug, Default, Clone, Copy)]
pub struct UlidGenerator;

impl IdGenerator for UlidGenerator {
    fn generate(&self) -> String {
        ulid::Ulid::new().to_string()
    }
}
