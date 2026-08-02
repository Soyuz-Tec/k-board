//! Ports: everything the engine refuses to decide for itself.
//!
//! The engine holds **no ambient authority**. It cannot read a clock, resolve
//! an identity, decide a permission, or reach a database. Each of those is a
//! trait the host implements, and that is the single property that lets one
//! codebase be both a standalone product and a component inside someone else's
//! platform.
//!
//! Embedded in K-Comms, [`Authority`] is a conversation-membership check and
//! [`OpLog`] is a Postgres table the host already owns. Standalone, they are
//! this crate's own adapters. The engine cannot tell the difference, and that
//! is deliberate: the day it can, it has stopped being embeddable.
//!
//! [`memory`] provides in-memory implementations. They exist for tests — the
//! engine's own and those of hosts integrating it — not for production.

use std::fmt;

use crate::clock::ActorId;
use crate::document::ScopeId;
use crate::op::StampedOp;
use crate::snapshot::Snapshot;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PortError {
    /// The host's authority port refused. The engine never second-guesses this.
    Denied,
    NotFound,
    /// Anything the host's storage wants to report. The engine does not
    /// interpret the message; it propagates it.
    Backend(String),
}

impl fmt::Display for PortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => formatter.write_str("denied by host authority"),
            Self::NotFound => formatter.write_str("not found"),
            Self::Backend(message) => write!(formatter, "backend: {message}"),
        }
    }
}

impl std::error::Error for PortError {}

/// Wall time in milliseconds.
///
/// The engine never calls `SystemTime::now`. Beyond determinism in tests, this
/// matters because an embedded engine has no business deciding whether time
/// comes from the OS, a test harness, or a host that virtualises it.
pub trait PhysicalClock {
    fn now_millis(&self) -> u64;
}

/// Who may read and write a scope.
///
/// The engine calls this and obeys the answer. It never inspects the actor, the
/// scope string, or any claim inside them — an embedded engine that formed its
/// own opinion about identity would be competing with its host's security
/// model, which is exactly the failure this design prevents.
pub trait Authority {
    fn may_read(&self, scope: &ScopeId, actor: ActorId) -> bool;
    fn may_write(&self, scope: &ScopeId, actor: ActorId) -> bool;
}

/// Durable, append-only operation storage.
///
/// Sequence numbers are the host's; the engine only requires that `append`
/// returns a monotonically increasing value per scope so `read_since` can page.
/// Ordering of the log does not affect the merged result — that is the CRDT's
/// job — but it does let a host resume a client cheaply.
pub trait OpLog {
    fn append(&mut self, scope: &ScopeId, ops: &[StampedOp]) -> Result<u64, PortError>;
    fn read_since(&self, scope: &ScopeId, after: u64) -> Result<Vec<StampedOp>, PortError>;
    fn count(&self, scope: &ScopeId) -> Result<u64, PortError>;
}

/// Materialised-document storage, so joining is not O(history).
pub trait SnapshotStore {
    fn load(&self, scope: &ScopeId) -> Result<Option<Snapshot>, PortError>;
    fn store(&mut self, scope: &ScopeId, snapshot: &Snapshot) -> Result<(), PortError>;
}

/// In-memory port implementations for tests.
pub mod memory {
    use std::cell::Cell;
    use std::collections::BTreeMap;

    use super::{Authority, OpLog, PhysicalClock, PortError, SnapshotStore};
    use crate::clock::ActorId;
    use crate::document::ScopeId;
    use crate::op::StampedOp;
    use crate::snapshot::Snapshot;

    /// A clock that never moves. Makes stamp ordering depend only on the
    /// logical counter, which is the harshest case for convergence.
    #[derive(Clone, Copy, Debug)]
    pub struct FixedClock(pub u64);

    impl PhysicalClock for FixedClock {
        fn now_millis(&self) -> u64 {
            self.0
        }
    }

    /// A clock that advances by a fixed step on every read.
    #[derive(Debug)]
    pub struct SteppingClock {
        now: Cell<u64>,
        step: u64,
    }

    impl SteppingClock {
        pub const fn new(start: u64, step: u64) -> Self {
            Self { now: Cell::new(start), step }
        }
    }

    impl PhysicalClock for SteppingClock {
        fn now_millis(&self) -> u64 {
            let current = self.now.get();
            self.now.set(current + self.step);
            current
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub struct AllowAll;

    impl Authority for AllowAll {
        fn may_read(&self, _scope: &ScopeId, _actor: ActorId) -> bool {
            true
        }
        fn may_write(&self, _scope: &ScopeId, _actor: ActorId) -> bool {
            true
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub struct DenyAll;

    impl Authority for DenyAll {
        fn may_read(&self, _scope: &ScopeId, _actor: ActorId) -> bool {
            false
        }
        fn may_write(&self, _scope: &ScopeId, _actor: ActorId) -> bool {
            false
        }
    }

    /// Grants access only to actors on a per-scope list — a stand-in for a
    /// host's membership check.
    #[derive(Clone, Debug, Default)]
    pub struct RosterAuthority {
        members: BTreeMap<ScopeId, Vec<ActorId>>,
    }

    impl RosterAuthority {
        pub fn admit(&mut self, scope: ScopeId, actor: ActorId) {
            self.members.entry(scope).or_default().push(actor);
        }
    }

    impl Authority for RosterAuthority {
        fn may_read(&self, scope: &ScopeId, actor: ActorId) -> bool {
            self.members.get(scope).is_some_and(|list| list.contains(&actor))
        }
        fn may_write(&self, scope: &ScopeId, actor: ActorId) -> bool {
            self.may_read(scope, actor)
        }
    }

    #[derive(Clone, Debug, Default)]
    pub struct MemoryOpLog {
        scopes: BTreeMap<ScopeId, Vec<StampedOp>>,
    }

    impl OpLog for MemoryOpLog {
        fn append(&mut self, scope: &ScopeId, ops: &[StampedOp]) -> Result<u64, PortError> {
            let log = self.scopes.entry(scope.clone()).or_default();
            log.extend_from_slice(ops);
            Ok(log.len() as u64)
        }

        fn read_since(&self, scope: &ScopeId, after: u64) -> Result<Vec<StampedOp>, PortError> {
            let Some(log) = self.scopes.get(scope) else {
                return Ok(Vec::new());
            };
            let start = (after as usize).min(log.len());
            Ok(log[start..].to_vec())
        }

        fn count(&self, scope: &ScopeId) -> Result<u64, PortError> {
            Ok(self.scopes.get(scope).map_or(0, Vec::len) as u64)
        }
    }

    #[derive(Clone, Debug, Default)]
    pub struct MemorySnapshots {
        stored: BTreeMap<ScopeId, Snapshot>,
    }

    impl SnapshotStore for MemorySnapshots {
        fn load(&self, scope: &ScopeId) -> Result<Option<Snapshot>, PortError> {
            Ok(self.stored.get(scope).cloned())
        }

        fn store(&mut self, scope: &ScopeId, snapshot: &Snapshot) -> Result<(), PortError> {
            if snapshot.scope() != scope {
                return Err(PortError::Backend("snapshot scope mismatch".into()));
            }
            self.stored.insert(scope.clone(), snapshot.clone());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::*;
    use super::*;
    use crate::clock::{ActorId, HlcGenerator};
    use crate::element::ElementId;
    use crate::op::upsert;
    use crate::prop::{PropKey, PropValue};

    fn scope() -> ScopeId {
        ScopeId::new("t/b")
    }

    #[test]
    fn roster_authority_scopes_access_per_tenant() {
        let mut roster = RosterAuthority::default();
        roster.admit(scope(), ActorId(1));

        assert!(roster.may_write(&scope(), ActorId(1)));
        assert!(!roster.may_write(&scope(), ActorId(2)), "non-member denied");
        assert!(
            !roster.may_read(&ScopeId::new("other/b"), ActorId(1)),
            "membership must not leak across scopes"
        );
    }

    #[test]
    fn op_log_pages_from_a_sequence() {
        let mut clock = HlcGenerator::new(ActorId(1));
        let mut log = MemoryOpLog::default();

        let first = upsert(ElementId(1), [(PropKey::X, PropValue::Num(1.0))], &mut clock, 10);
        let seq = log.append(&scope(), &first).unwrap();

        let second = upsert(ElementId(2), [(PropKey::X, PropValue::Num(2.0))], &mut clock, 11);
        log.append(&scope(), &second).unwrap();

        assert_eq!(log.read_since(&scope(), seq).unwrap(), second);
        assert_eq!(log.count(&scope()).unwrap(), 2);
    }

    #[test]
    fn op_log_isolates_scopes() {
        let mut clock = HlcGenerator::new(ActorId(1));
        let mut log = MemoryOpLog::default();
        let ops = upsert(ElementId(1), [(PropKey::X, PropValue::Num(1.0))], &mut clock, 10);
        log.append(&ScopeId::new("tenant-a/b"), &ops).unwrap();

        assert!(log.read_since(&ScopeId::new("tenant-b/b"), 0).unwrap().is_empty());
    }

    #[test]
    fn snapshot_store_rejects_a_mismatched_scope() {
        use crate::snapshot::Snapshot;
        let mut store = MemorySnapshots::default();
        let snapshot = Snapshot::empty(ScopeId::new("tenant-a/b"));
        assert!(store.store(&ScopeId::new("tenant-b/b"), &snapshot).is_err());
    }

    #[test]
    fn stepping_clock_advances() {
        let clock = SteppingClock::new(100, 5);
        assert_eq!(clock.now_millis(), 100);
        assert_eq!(clock.now_millis(), 105);
    }
}
