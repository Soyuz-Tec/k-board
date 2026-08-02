//! Hybrid logical clock.
//!
//! Every convergent value in this engine is stamped with an [`Hlc`]. The stamp
//! provides a *total* order across replicas, which is what makes merge
//! deterministic: given the same set of stamped values, every replica computes
//! the same result regardless of the order they arrived in.
//!
//! Wall time is supplied by the host through [`crate::ports::PhysicalClock`].
//! The engine never reads a clock itself — that is part of holding no ambient
//! authority, and it is also what makes these tests deterministic.

use core::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Opaque actor identity.
///
/// The engine never interprets this value. Hosts assign it and are responsible
/// for uniqueness within a scope's lifetime — K-Comms would derive it per
/// session, a standalone deployment per connection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ActorId(pub u64);

/// A hybrid logical clock stamp.
///
/// Ordering is lexicographic over `(wall, counter, actor)`. Including `actor`
/// makes the order *total* rather than partial, so concurrent edits never tie.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Hlc {
    /// Host-supplied physical time, milliseconds.
    pub wall: u64,
    /// Disambiguates events within the same millisecond.
    pub counter: u32,
    /// Breaks ties between replicas. Never a "winner" in any meaningful sense —
    /// only a deterministic choice.
    pub actor: ActorId,
}

impl Hlc {
    /// The stamp that precedes every other stamp for `actor`.
    pub const fn zero(actor: ActorId) -> Self {
        Self {
            wall: 0,
            counter: 0,
            actor,
        }
    }
}

impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        // Written explicitly rather than derived: the field order *is* the
        // semantics, and a future field reorder must not silently change it.
        self.wall
            .cmp(&other.wall)
            .then(self.counter.cmp(&other.counter))
            .then(self.actor.cmp(&other.actor))
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Generates monotonically increasing stamps for one actor.
///
/// A replica keeps exactly one generator. It advances on local edits
/// ([`tick`](Self::tick)) and on observing remote stamps
/// ([`observe`](Self::observe)), which is what keeps causality intact when a
/// host's wall clock lags behind a peer's.
#[derive(Clone, Debug)]
pub struct HlcGenerator {
    last: Hlc,
}

impl HlcGenerator {
    pub const fn new(actor: ActorId) -> Self {
        Self {
            last: Hlc::zero(actor),
        }
    }

    pub const fn actor(&self) -> ActorId {
        self.last.actor
    }

    pub const fn last(&self) -> Hlc {
        self.last
    }

    /// Stamp a local event.
    pub fn tick(&mut self, physical_now: u64) -> Hlc {
        let wall = physical_now.max(self.last.wall);
        let counter = if wall == self.last.wall {
            // Same millisecond (or a clock that went backwards): keep ordering
            // by advancing the logical counter instead.
            self.last.counter.saturating_add(1)
        } else {
            0
        };
        self.last = Hlc {
            wall,
            counter,
            actor: self.last.actor,
        };
        self.last
    }

    /// Stamp a local event that causally follows an observed remote stamp.
    ///
    /// This is the step that guarantees a reply is always ordered after the
    /// thing it replies to, even when the local wall clock is behind.
    pub fn observe(&mut self, remote: Hlc, physical_now: u64) -> Hlc {
        let wall = physical_now.max(self.last.wall).max(remote.wall);
        let counter = match (wall == self.last.wall, wall == remote.wall) {
            (true, true) => self.last.counter.max(remote.counter).saturating_add(1),
            (true, false) => self.last.counter.saturating_add(1),
            (false, true) => remote.counter.saturating_add(1),
            (false, false) => 0,
        };
        self.last = Hlc {
            wall,
            counter,
            actor: self.last.actor,
        };
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ActorId = ActorId(1);
    const B: ActorId = ActorId(2);

    #[test]
    fn ordering_is_total_across_actors() {
        let x = Hlc {
            wall: 5,
            counter: 0,
            actor: A,
        };
        let y = Hlc {
            wall: 5,
            counter: 0,
            actor: B,
        };
        assert_ne!(
            x.cmp(&y),
            Ordering::Equal,
            "equal wall+counter must still order"
        );
        assert!(x < y);
    }

    #[test]
    fn wall_dominates_counter_and_actor() {
        let earlier = Hlc {
            wall: 5,
            counter: 999,
            actor: B,
        };
        let later = Hlc {
            wall: 6,
            counter: 0,
            actor: A,
        };
        assert!(earlier < later);
    }

    #[test]
    fn tick_is_monotonic_within_a_millisecond() {
        let mut gen = HlcGenerator::new(A);
        let first = gen.tick(100);
        let second = gen.tick(100);
        let third = gen.tick(100);
        assert!(first < second && second < third);
        assert_eq!(third.counter, 2);
    }

    #[test]
    fn tick_survives_a_clock_moving_backwards() {
        let mut gen = HlcGenerator::new(A);
        let first = gen.tick(100);
        // NTP correction, container migration, a laptop waking up.
        let second = gen.tick(40);
        assert!(
            second > first,
            "a backwards host clock must not break ordering"
        );
        assert_eq!(second.wall, 100);
    }

    #[test]
    fn observe_orders_after_a_remote_stamp_from_the_future() {
        let mut gen = HlcGenerator::new(A);
        gen.tick(100);
        let remote = Hlc {
            wall: 5_000,
            counter: 3,
            actor: B,
        };
        let reply = gen.observe(remote, 101);
        assert!(
            reply > remote,
            "a causal reply must outrank what it replies to"
        );
        assert_eq!(reply.wall, 5_000);
        assert_eq!(reply.counter, 4);
    }
}
