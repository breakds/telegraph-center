//! Pure login-throttling policy.
//!
//! The decision is a function of how many recent failures match the attempted
//! username or remote IP. The 15-minute counting window is applied by the caller
//! when it queries the failure count, so the lock lifts automatically as old
//! failures age out of the window.

use time::Duration;

/// Failures (within the window) at which a fixed delay is applied.
pub const DELAY_THRESHOLD: i64 = 5;

/// Failures (within the window) at which login is locked out entirely.
pub const LOCK_THRESHOLD: i64 = 10;

/// How long to delay an attempt once the delay threshold is reached.
pub const DELAY: Duration = Duration::seconds(2);

/// What to do with a login attempt given the recent failure count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleDecision {
    /// Process the attempt immediately.
    Allow,
    /// Wait `Duration`, then process the attempt.
    Delay(Duration),
    /// Reject the attempt without checking credentials.
    Locked,
}

/// The login-throttling policy.
pub struct LoginThrottle;

impl LoginThrottle {
    /// Decide how to handle an attempt given the number of failures counted
    /// within the throttling window.
    pub fn decision(failure_count: i64) -> ThrottleDecision {
        if failure_count >= LOCK_THRESHOLD {
            ThrottleDecision::Locked
        } else if failure_count >= DELAY_THRESHOLD {
            ThrottleDecision::Delay(DELAY)
        } else {
            ThrottleDecision::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_below_the_delay_threshold() {
        assert_eq!(LoginThrottle::decision(0), ThrottleDecision::Allow);
        assert_eq!(LoginThrottle::decision(4), ThrottleDecision::Allow);
    }

    #[test]
    fn delays_at_the_delay_threshold() {
        assert_eq!(LoginThrottle::decision(5), ThrottleDecision::Delay(DELAY));
        assert_eq!(LoginThrottle::decision(9), ThrottleDecision::Delay(DELAY));
    }

    #[test]
    fn locks_at_the_lock_threshold() {
        assert_eq!(LoginThrottle::decision(10), ThrottleDecision::Locked);
        assert_eq!(LoginThrottle::decision(50), ThrottleDecision::Locked);
    }
}
