//! Last-writer-wins register.
//!
//! The smallest convergent unit in the engine. Because [`Hlc`] is totally
//! ordered, [`Lww::merge`] is commutative, associative, and idempotent — the
//! three properties that let replicas exchange state in any order, more than
//! once, and still agree.
//!
//! This is deliberately applied *per property* rather than per element. Merging
//! whole elements is what makes editors lose one of two concurrent edits when
//! two people touch the same shape; see `document.rs` for the consequence.

use serde::{Deserialize, Serialize};

use crate::clock::Hlc;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Lww<T> {
    value: T,
    stamp: Hlc,
}

impl<T> Lww<T> {
    pub const fn new(value: T, stamp: Hlc) -> Self {
        Self { value, stamp }
    }

    pub const fn get(&self) -> &T {
        &self.value
    }

    pub const fn stamp(&self) -> Hlc {
        self.stamp
    }

    pub fn into_inner(self) -> T {
        self.value
    }

    /// Write locally. Returns `true` if the write took effect.
    ///
    /// A stamp that does not beat the current one is dropped. That is not an
    /// error: it is a stale write, and dropping it is the convergent outcome.
    pub fn set(&mut self, value: T, stamp: Hlc) -> bool {
        if stamp > self.stamp {
            self.value = value;
            self.stamp = stamp;
            true
        } else {
            false
        }
    }

    /// Merge a remote register. Returns `true` if this register changed.
    ///
    /// Strict `>` is what makes this idempotent: merging the same value twice
    /// is a no-op the second time.
    pub fn merge(&mut self, other: &Self) -> bool
    where
        T: Clone,
    {
        if other.stamp > self.stamp {
            self.value = other.value.clone();
            self.stamp = other.stamp;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ActorId;

    const A: ActorId = ActorId(1);
    const B: ActorId = ActorId(2);

    fn stamp(wall: u64, actor: ActorId) -> Hlc {
        Hlc { wall, counter: 0, actor }
    }

    #[test]
    fn later_stamp_wins() {
        let mut left = Lww::new("old", stamp(1, A));
        let right = Lww::new("new", stamp(2, B));
        assert!(left.merge(&right));
        assert_eq!(*left.get(), "new");
    }

    #[test]
    fn earlier_stamp_is_dropped() {
        let mut left = Lww::new("current", stamp(5, A));
        let right = Lww::new("stale", stamp(2, B));
        assert!(!left.merge(&right));
        assert_eq!(*left.get(), "current");
    }

    #[test]
    fn merge_is_commutative() {
        let one = Lww::new("a", stamp(3, A));
        let two = Lww::new("b", stamp(3, B));

        let mut forward = one.clone();
        forward.merge(&two);
        let mut backward = two.clone();
        backward.merge(&one);

        assert_eq!(forward, backward);
    }

    #[test]
    fn merge_is_idempotent() {
        let mut left = Lww::new("a", stamp(1, A));
        let right = Lww::new("b", stamp(2, B));

        left.merge(&right);
        let once = left.clone();
        assert!(!left.merge(&right), "second merge must be a no-op");
        assert_eq!(left, once);
    }

    #[test]
    fn concurrent_equal_wall_times_resolve_by_actor() {
        // Same millisecond, no causal relationship. Someone has to win, and it
        // has to be the same someone on every replica.
        let mut left = Lww::new("from_a", stamp(7, A));
        let right = Lww::new("from_b", stamp(7, B));
        left.merge(&right);
        assert_eq!(*left.get(), "from_b", "higher actor id breaks the tie");
    }
}
