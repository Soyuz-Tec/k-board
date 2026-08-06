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

/// One request per admitted scope connection can be pending at a time.
pub const CELL_MAILBOX_CAPACITY: usize = 64;

/// Shared SQLite writer queue. It is bounded independently of room mailboxes.
pub const STORAGE_MAILBOX_CAPACITY: usize = 64;

/// Caller wait bounds. A room command can outlive the transport caller. Storage
/// commands may time out only while still queued; once SQLite work starts the
/// room waits for its definitive outcome before applying, acknowledging or
/// retrying it.
pub const COMMAND_DEADLINE: Duration = Duration::from_secs(5);
#[cfg(not(test))]
pub const STORAGE_DEADLINE: Duration = Duration::from_secs(5);

/// Sustained frames per second per connection, and the burst allowance.
///
/// Two streams share this budget. The client commits a drag at most every 50ms
/// (20/s) and reports its cursor at most every 60ms (~17/s), so a user drawing
/// as fast as the client will emit sends about 37/s. 60/s leaves headroom for a
/// fast stylus while still capping a hostile peer.
pub const RATE_PER_SECOND: f64 = 60.0;
pub const RATE_BURST: f64 = 120.0;

/// How many frames a connection may exceed its budget before being closed.
/// A few rejections are transient; a persistent flood is not.
pub const MAX_RATE_STRIKES: u32 = 20;

/// Aggregate budgets prevent many sockets from multiplying the connection
/// budget. Values leave room for several legitimate collaborators per board.
pub const IDENTITY_RATE_PER_SECOND: f64 = 120.0;
pub const IDENTITY_RATE_BURST: f64 = 240.0;
pub const SCOPE_RATE_PER_SECOND: f64 = 600.0;
pub const SCOPE_RATE_BURST: f64 = 1_200.0;

pub const MAX_CONNECTIONS: usize = 20_000;
pub const MAX_CONNECTIONS_PER_SCOPE: usize = CELL_MAILBOX_CAPACITY;
pub const MAX_SCOPES_PER_IDENTITY: usize = 8;

/// Largest number of live elements in one room.
pub const MAX_ELEMENTS_PER_ROOM: usize = 50_000;

/// Bounds materialized state, snapshots, join serialization and simultaneous
/// restore memory. These include tombstones.
pub const MAX_ROOM_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SERIALIZED_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CONCURRENT_RESTORES: usize = 4;

/// Largest number of rooms held concurrently. Rooms are created by URL path, so
/// without this any visitor can mint unbounded state.
pub const MAX_ROOMS: usize = 10_000;
pub const MAX_RESTORING_ROOMS: usize = 32;
pub const MAX_FAILED_ROOMS: usize = 256;
pub const FAILED_RESTORE_RETRY: Duration = Duration::from_secs(5);
pub const FAILED_ENTRY_TTL: Duration = Duration::from_secs(5 * 60);

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

/// Token bucket used independently for connection, identity and scope budgets.
/// Each layer has its own configured rate; no process-global traffic bucket lets
/// one abusive scope consume every unrelated scope's allowance.
#[derive(Debug)]
pub struct RateLimiter {
    rate_per_second: f64,
    burst: f64,
    tokens: f64,
    last_refill: Instant,
    strikes: u32,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::with_budget(RATE_PER_SECOND, RATE_BURST)
    }

    pub fn with_budget(rate_per_second: f64, burst: f64) -> Self {
        Self {
            rate_per_second,
            burst,
            tokens: burst,
            last_refill: Instant::now(),
            strikes: 0,
        }
    }

    /// Charge one frame. `false` means the caller should drop it.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.rate_per_second).min(self.burst);

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
