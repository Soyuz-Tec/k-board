//! The document: a convergent map of elements within one tenant scope.
//!
//! A document is an LWW-map of elements, each of which is itself an LWW-map of
//! properties. Nesting convergent structures this way keeps the whole document
//! convergent — merge remains commutative, associative, and idempotent, which
//! is what lets replicas gossip in any order, over unreliable transports, more
//! than once.
//!
//! Tenancy is enforced here and nowhere else in the engine: a document carries
//! an opaque [`ScopeId`] and refuses to merge with a document from a different
//! scope. The engine never parses that identifier — it only checks equality.
//! Deciding *who* may open a scope is the host's job.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::clock::Hlc;
use crate::element::{Element, ElementId};
use crate::frac;
use crate::prop::{PropKey, PropValue};

/// Opaque tenant/board scope, assigned by the host.
///
/// K-Comms would use a tenant+conversation composite; a standalone deployment
/// would use its own board identifier. The engine treats it as bytes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ScopeId(pub String);

impl ScopeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for ScopeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MergeError {
    /// The single most dangerous bug a multi-tenant engine can have, made
    /// unrepresentable rather than merely tested for.
    ScopeMismatch { expected: ScopeId, found: ScopeId },
}

impl fmt::Display for MergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScopeMismatch { expected, found } => {
                write!(
                    formatter,
                    "refused cross-scope merge: {expected} != {found}"
                )
            }
        }
    }
}

impl std::error::Error for MergeError {}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Document {
    scope: ScopeId,
    elements: BTreeMap<ElementId, Element>,
}

impl Document {
    pub fn new(scope: ScopeId) -> Self {
        Self {
            scope,
            elements: BTreeMap::new(),
        }
    }

    pub const fn scope(&self) -> &ScopeId {
        &self.scope
    }

    pub fn get(&self, id: ElementId) -> Option<&Element> {
        self.elements.get(&id)
    }

    pub fn get_mut(&mut self, id: ElementId) -> Option<&mut Element> {
        self.elements.get_mut(&id)
    }

    /// Insert or return the existing element. Elements are created empty; every
    /// attribute arrives as a stamped property write.
    pub fn entry(&mut self, id: ElementId) -> &mut Element {
        self.elements.entry(id).or_insert_with(|| Element::new(id))
    }

    /// Live elements, in paint order.
    pub fn ordered(&self) -> Vec<&Element> {
        let mut live: Vec<&Element> = self.elements.values().filter(|e| !e.is_deleted()).collect();
        // Element id breaks ties so the order is total even if two elements
        // somehow carry the same fractional key.
        live.sort_by(|left, right| {
            left.z_index()
                .cmp(right.z_index())
                .then(left.id().cmp(&right.id()))
        });
        live
    }

    pub fn live(&self) -> impl Iterator<Item = &Element> {
        self.elements
            .values()
            .filter(|element| !element.is_deleted())
    }

    /// Includes tombstones. Snapshotting and garbage collection need these;
    /// rendering does not.
    pub fn all(&self) -> impl Iterator<Item = &Element> {
        self.elements.values()
    }

    pub fn live_count(&self) -> usize {
        self.live().count()
    }

    pub fn total_count(&self) -> usize {
        self.elements.len()
    }

    /// Conservative materialized payload estimate for host resource budgets.
    /// This includes tombstones because they consume snapshot and restore work.
    pub fn estimated_payload_bytes(&self) -> usize {
        self.elements.values().fold(0_usize, |document, element| {
            element
                .props()
                .fold(document.saturating_add(64), |bytes, (key, value)| {
                    bytes
                        .saturating_add(key.estimated_bytes())
                        .saturating_add(value.estimated_bytes())
                        .saturating_add(32)
                })
        })
    }

    /// Highest stamp anywhere in the document.
    pub fn max_stamp(&self) -> Option<Hlc> {
        self.elements.values().filter_map(Element::max_stamp).max()
    }

    /// A z-key that places a new element above everything currently live.
    pub fn z_index_for_top(&self) -> String {
        let highest = self
            .ordered()
            .last()
            .map(|element| element.z_index().to_owned());
        frac::between(highest.as_deref().filter(|key| !key.is_empty()), None)
    }

    /// Merge another replica's document.
    ///
    /// # Errors
    ///
    /// [`MergeError::ScopeMismatch`] if the documents belong to different
    /// tenants. This is a hard refusal rather than a filter: a caller that
    /// reaches this point has a routing bug, and silently dropping the data
    /// would hide it.
    pub fn merge(&mut self, other: &Self) -> Result<bool, MergeError> {
        if self.scope != other.scope {
            return Err(MergeError::ScopeMismatch {
                expected: self.scope.clone(),
                found: other.scope.clone(),
            });
        }

        let mut changed = false;
        for (id, incoming) in &other.elements {
            match self.elements.get_mut(id) {
                Some(current) => changed |= current.merge(incoming),
                None => {
                    // Route even a whole incoming element through the same
                    // validation as an existing one. Deserialised peer state
                    // is a trust-boundary input, not permission to bypass the
                    // property contract.
                    let mut validated = Element::new(*id);
                    if validated.merge(incoming) {
                        self.elements.insert(*id, validated);
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }

    /// Tombstone an element.
    pub fn delete(&mut self, id: ElementId, stamp: Hlc) -> bool {
        self.entry(id)
            .set(PropKey::Deleted, PropValue::Bool(true), stamp)
    }

    /// Permanently drop tombstones whose last write is older than `before`.
    ///
    /// Only safe once every replica is known to have observed `before` —
    /// that judgement belongs to the host, which is why this is explicit rather
    /// than automatic. Collecting too early resurrects deleted elements.
    pub fn collect_tombstones(&mut self, before: Hlc) -> usize {
        let collectable: Vec<ElementId> = self
            .elements
            .values()
            .filter(|element| {
                element.is_deleted() && element.max_stamp().is_some_and(|stamp| stamp < before)
            })
            .map(Element::id)
            .collect();

        for id in &collectable {
            self.elements.remove(id);
        }
        collectable.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ActorId;
    use crate::prop::ElementKind;

    const A: ActorId = ActorId(1);
    const B: ActorId = ActorId(2);

    fn stamp(wall: u64, actor: ActorId) -> Hlc {
        Hlc {
            wall,
            counter: 0,
            actor,
        }
    }

    fn scope() -> ScopeId {
        ScopeId::new("tenant-1/board-9")
    }

    fn with_rect(document: &mut Document, id: u128, x: f64, at: Hlc) {
        let element = document.entry(ElementId(id));
        element.set(PropKey::Kind, PropValue::Kind(ElementKind::Rectangle), at);
        element.set(PropKey::X, PropValue::Num(x), at);
    }

    #[test]
    fn cross_tenant_merge_is_refused() {
        let mut mine = Document::new(ScopeId::new("tenant-a/board-1"));
        let theirs = Document::new(ScopeId::new("tenant-b/board-1"));
        assert!(matches!(
            mine.merge(&theirs),
            Err(MergeError::ScopeMismatch { .. })
        ));
    }

    #[test]
    fn merge_is_commutative_across_replicas() {
        let mut left = Document::new(scope());
        with_rect(&mut left, 1, 10.0, stamp(1, A));

        let mut right = Document::new(scope());
        with_rect(&mut right, 2, 20.0, stamp(1, B));

        let mut forward = left.clone();
        forward.merge(&right).unwrap();
        let mut backward = right.clone();
        backward.merge(&left).unwrap();

        assert_eq!(forward, backward);
        assert_eq!(forward.live_count(), 2);
    }

    #[test]
    fn tombstones_survive_a_concurrent_edit() {
        // A deletes; B edits the same element without having seen the delete.
        // The delete is later, so the element stays gone — but B's edit is not
        // lost, it is merged into a tombstoned element and reappears only if
        // something later undeletes it.
        let mut replica = Document::new(scope());
        with_rect(&mut replica, 1, 10.0, stamp(1, A));

        let mut editor = replica.clone();
        editor
            .entry(ElementId(1))
            .set(PropKey::X, PropValue::Num(99.0), stamp(5, B));

        replica.delete(ElementId(1), stamp(9, A));
        replica.merge(&editor).unwrap();

        assert_eq!(replica.live_count(), 0);
        assert_eq!(replica.total_count(), 1, "tombstone must be retained");
    }

    #[test]
    fn deleting_an_unknown_element_is_safe() {
        // A delete can arrive before the create it refers to.
        let mut document = Document::new(scope());
        document.delete(ElementId(42), stamp(3, A));

        let mut creator = Document::new(scope());
        with_rect(&mut creator, 42, 5.0, stamp(1, B));
        document.merge(&creator).unwrap();

        assert_eq!(document.live_count(), 0, "the later delete still wins");
    }

    #[test]
    fn paint_order_follows_the_fractional_index() {
        let mut document = Document::new(scope());
        with_rect(&mut document, 1, 0.0, stamp(1, A));
        let bottom = document.z_index_for_top();
        document
            .entry(ElementId(1))
            .set(PropKey::ZIndex, PropValue::Text(bottom), stamp(2, A));

        with_rect(&mut document, 2, 0.0, stamp(3, A));
        let top = document.z_index_for_top();
        document
            .entry(ElementId(2))
            .set(PropKey::ZIndex, PropValue::Text(top), stamp(4, A));

        let order: Vec<u128> = document.ordered().iter().map(|e| e.id().0).collect();
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn tombstone_collection_respects_the_horizon() {
        let mut document = Document::new(scope());
        with_rect(&mut document, 1, 0.0, stamp(1, A));
        document.delete(ElementId(1), stamp(5, A));

        assert_eq!(
            document.collect_tombstones(stamp(3, A)),
            0,
            "too early to collect"
        );
        assert_eq!(document.collect_tombstones(stamp(50, A)), 1);
        assert_eq!(document.total_count(), 0);
    }
}
