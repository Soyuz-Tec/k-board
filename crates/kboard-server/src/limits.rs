//! Resource limits for the standalone server.
//!
//! Every value here exists because without it a single unauthenticated client
//! can drive the process to exhaustion. They are gathered in one module so the
//! deployed posture is auditable by reading one file rather than by grepping
//! for magic numbers.
//!
//! These are *server* limits, not engine limits. The engine is deliberately
//! unopinionated about capacity; a host embedding it enforces its own budget.
//! See `docs/adr/0006-server-resource-limits.md`.

use std::time::{Duration, Instant};

/// Largest accepted WebSocket text frame.
///
/// Comfortably above a legitimate batch (a freehand stroke with a few thousand
/// points) and far below anything that pressures memory.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Largest number of operations in one batch.
pub const MAX_OPS_PER_FRAME: usize = 512;

/// Sustained frames per second per connection, and the burst allowance.
///
/// The client commits a drag at most every 50ms (20/s), so 60/s leaves ample
/// headroom for a fast stylus while capping a hostile peer.
pub const RATE_PER_SECOND: f64 = 60.0;
pub const RATE_BURST: f64 = 120.0;

/// How many frames a connection may exceed its budget before being closed.
/// A few rejections are transient; a persistent flood is not.
pub const MAX_RATE_STRIKES: u32 = 20;

/// Largest number of live elements in one room.
pub const MAX_ELEMENTS_PER_ROOM: usize = 50_000;

/// Largest number of rooms held concurrently. Rooms are created by URL path, so
/// without this any visitor can mint unbounded state.
pub const MAX_ROOMS: usize = 10_000;

/// Longest accepted scope identifier.
pub const MAX_SCOPE_BYTES: usize = 128;

/// How long an empty room is retained before reclamation.
pub const ROOM_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// How often the reclamation sweep runs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Scope identifiers are opaque to the engine, but the server still constrains
/// them: they arrive in a URL path, become map keys, and are echoed into
/// responses. Restricting the charset removes path-traversal, log-injection,
/// and unbounded-key concerns in one step.
pub fn scope_is_acceptable(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= MAX_SCOPE_BYTES
        && scope
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// Token bucket, one per connection.
///
/// Deliberately not shared across connections: a global limiter would let one
/// abusive peer degrade everyone, which is the failure it exists to prevent.
#[derive(Debug)]
pub struct RateLimiter {
    tokens: f64,
    last_refill: Instant,
    strikes: u32,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            tokens: RATE_BURST,
            last_refill: Instant::now(),
            strikes: 0,
        }
    }

    /// Charge one frame. `false` means the caller should drop it.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * RATE_PER_SECOND).min(RATE_BURST);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            // A well-behaved burst should not count against a client forever.
            self.strikes = self.strikes.saturating_sub(1);
            true
        } else {
            self.strikes += 1;
            false
        }
    }

    /// Whether the connection has misbehaved persistently enough to close.
    pub fn should_disconnect(&self) -> bool {
        self.strikes >= MAX_RATE_STRIKES
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_charset_is_constrained() {
        assert!(scope_is_acceptable("tenant-1:board_9.v2"));
        assert!(!scope_is_acceptable(""), "empty scope");
        assert!(!scope_is_acceptable("../../etc/passwd"), "path traversal");
        assert!(!scope_is_acceptable("has space"));
        assert!(!scope_is_acceptable("newline\ninjection"));
        assert!(
            !scope_is_acceptable(&"x".repeat(MAX_SCOPE_BYTES + 1)),
            "too long"
        );
    }

    #[test]
    fn burst_is_allowed_then_throttled() {
        let mut limiter = RateLimiter::new();
        // The full burst allowance passes.
        for index in 0..(RATE_BURST as usize) {
            assert!(limiter.allow(), "frame {index} within burst");
        }
        // The next is refused: refill over microseconds is negligible.
        assert!(!limiter.allow(), "burst must be capped");
    }

    #[test]
    fn a_sustained_flood_eventually_disconnects() {
        let mut limiter = RateLimiter::new();
        for _ in 0..(RATE_BURST as usize) {
            limiter.allow();
        }
        assert!(!limiter.should_disconnect(), "a burst alone is not abuse");
        for _ in 0..MAX_RATE_STRIKES {
            limiter.allow();
        }
        assert!(limiter.should_disconnect(), "a persistent flood is");
    }
}
