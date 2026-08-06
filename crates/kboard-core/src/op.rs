//! Operations: the unit of exchange between replicas and the unit of durability
//! in a host's log.
//!
//! An operation is a stamped property write or a stamped tombstone. That is the
//! whole vocabulary. Everything a user does — draw, drag, restyle, reorder,
//! group, erase — reduces to these, which is what keeps the wire format stable
//! while the editor above it changes.
//!
//! There is deliberately no `Clear` operation. See [`clear`] for why.

use serde::{Deserialize, Serialize};

use crate::clock::{Hlc, HlcGenerator};
use crate::document::Document;
use crate::element::ElementId;
use crate::prop::{PropKey, PropValue};

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Set {
        element: ElementId,
        key: PropKey,
        value: PropValue,
    },
    Delete {
        element: ElementId,
    },
}

impl Op {
    pub const fn element(&self) -> ElementId {
        match self {
            Self::Set { element, .. } | Self::Delete { element } => *element,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct StampedOp {
    pub stamp: Hlc,
    pub op: Op,
}

impl StampedOp {
    pub const fn new(stamp: Hlc, op: Op) -> Self {
        Self { stamp, op }
    }

    pub const fn element(&self) -> ElementId {
        self.op.element()
    }

    /// Whether every value carried by this operation is safe to absorb.
    ///
    /// Hosts use this at trust boundaries before persistence. The element
    /// still enforces the same rule on write as defence in depth.
    pub fn is_valid(&self) -> bool {
        match &self.op {
            Op::Set { key, value, .. } => value.is_valid_for(key),
            Op::Delete { .. } => true,
        }
    }

    /// Conservative payload size for host-side aggregate budgets.
    pub fn estimated_bytes(&self) -> usize {
        let operation = match &self.op {
            Op::Set { key, value, .. } => key
                .estimated_bytes()
                .saturating_add(value.estimated_bytes()),
            Op::Delete { .. } => 16,
        };
        48_usize.saturating_add(operation)
    }
}

/// Apply one operation. Returns `true` if the document changed.
///
/// A `false` return is normal — it means the operation was stale or already
/// applied. Callers replaying a log can ignore it; callers deciding whether to
/// broadcast should not.
pub fn apply(document: &mut Document, stamped: &StampedOp) -> bool {
    match &stamped.op {
        Op::Set {
            element,
            key,
            value,
        } => {
            // Validate before `entry`: creating an empty element for a refused
            // property would still consume room capacity and snapshot space.
            value.is_valid_for(key)
                && document
                    .entry(*element)
                    .set(key.clone(), value.clone(), stamped.stamp)
        }
        Op::Delete { element } => document.delete(*element, stamped.stamp),
    }
}

/// Apply a batch in the order given. Returns the number of operations that
/// changed the document.
///
/// Order does not affect the result — that is the point of the CRDT — but
/// applying in log order keeps the change count meaningful for hosts that
/// broadcast only on change.
pub fn apply_all(document: &mut Document, ops: &[StampedOp]) -> usize {
    ops.iter()
        .filter(|stamped| apply(document, stamped))
        .count()
}

/// Expand "clear the board" into explicit tombstones at the originating replica.
///
/// Modelling clear as its own operation looks simpler and is wrong. Each
/// replica would expand it against whatever elements *it* knows about, so a
/// replica that had not yet seen an element would not tombstone it while
/// another would — and the two would never converge.
///
/// Expanding at the origin makes clear an ordinary, deterministic set of
/// deletes. An element the origin had not yet seen survives the clear, which is
/// both convergent and the behaviour a user expects: you cannot erase something
/// that had not reached you.
pub fn clear(document: &Document, clock: &mut HlcGenerator, now_millis: u64) -> Vec<StampedOp> {
    document
        .live()
        .map(|element| {
            StampedOp::new(
                clock.tick(now_millis),
                Op::Delete {
                    element: element.id(),
                },
            )
        })
        .collect()
}

/// Build the operations that create a shape, ready to be stamped and logged.
///
/// A convenience for hosts and tests; the engine imposes no required property
/// set beyond what the renderer needs to draw something.
pub fn upsert(
    element: ElementId,
    props: impl IntoIterator<Item = (PropKey, PropValue)>,
    clock: &mut HlcGenerator,
    now_millis: u64,
) -> Vec<StampedOp> {
    props
        .into_iter()
        .map(|(key, value)| {
            StampedOp::new(
                clock.tick(now_millis),
                Op::Set {
                    element,
                    key,
                    value,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ActorId;
    use crate::document::ScopeId;
    use crate::prop::ElementKind;

    fn scope() -> ScopeId {
        ScopeId::new("t/b")
    }

    #[test]
    fn upsert_then_apply_builds_the_shape() {
        let mut clock = HlcGenerator::new(ActorId(1));
        let ops = upsert(
            ElementId(1),
            [
                (PropKey::Kind, PropValue::Kind(ElementKind::Ellipse)),
                (PropKey::X, PropValue::Num(4.0)),
            ],
            &mut clock,
            1_000,
        );

        let mut document = Document::new(scope());
        assert_eq!(apply_all(&mut document, &ops), 2);
        assert_eq!(
            document.get(ElementId(1)).unwrap().kind(),
            Some(ElementKind::Ellipse)
        );
    }

    #[test]
    fn replaying_a_log_twice_changes_nothing() {
        let mut clock = HlcGenerator::new(ActorId(1));
        let ops = upsert(
            ElementId(1),
            [(PropKey::X, PropValue::Num(4.0))],
            &mut clock,
            1_000,
        );

        let mut document = Document::new(scope());
        apply_all(&mut document, &ops);
        let after_first = document.clone();

        assert_eq!(apply_all(&mut document, &ops), 0, "replay must be a no-op");
        assert_eq!(document, after_first);
    }

    #[test]
    fn semantic_validation_is_available_before_application() {
        let invalid = StampedOp::new(
            HlcGenerator::new(ActorId(1)).tick(1_000),
            Op::Set {
                element: ElementId(1),
                key: PropKey::Text,
                value: PropValue::Text("x".repeat(crate::prop::MAX_TEXT_BYTES + 1)),
            },
        );
        let deletion = StampedOp::new(
            HlcGenerator::new(ActorId(1)).tick(1_001),
            Op::Delete {
                element: ElementId(1),
            },
        );
        let wrong_type = StampedOp::new(
            HlcGenerator::new(ActorId(1)).tick(1_002),
            Op::Set {
                element: ElementId(1),
                key: PropKey::Deleted,
                value: PropValue::Text("false".into()),
            },
        );

        assert!(!invalid.is_valid());
        assert!(deletion.is_valid());
        assert!(!wrong_type.is_valid());

        let mut document = Document::new(scope());
        assert!(!apply(&mut document, &wrong_type));
        assert_eq!(
            document.total_count(),
            0,
            "a refused property must not leave an empty element"
        );
    }

    #[test]
    fn clear_does_not_erase_elements_the_origin_never_saw() {
        let mut clock_a = HlcGenerator::new(ActorId(1));
        let mut clock_b = HlcGenerator::new(ActorId(2));

        let mut origin = Document::new(scope());
        apply_all(
            &mut origin,
            &upsert(
                ElementId(1),
                [(PropKey::X, PropValue::Num(1.0))],
                &mut clock_a,
                10,
            ),
        );

        // A second replica adds an element the origin has not received.
        let mut peer = origin.clone();
        let unseen = upsert(
            ElementId(2),
            [(PropKey::X, PropValue::Num(2.0))],
            &mut clock_b,
            11,
        );
        apply_all(&mut peer, &unseen);

        let clear_ops = clear(&origin, &mut clock_a, 20);
        apply_all(&mut origin, &clear_ops);
        origin.merge(&peer).unwrap();

        assert_eq!(
            origin.live_count(),
            1,
            "the unseen element survives the clear"
        );
        assert!(origin.get(ElementId(2)).is_some_and(|e| !e.is_deleted()));
    }

    #[test]
    fn clear_converges_regardless_of_arrival_order() {
        let mut clock = HlcGenerator::new(ActorId(1));
        let mut document = Document::new(scope());
        apply_all(
            &mut document,
            &upsert(
                ElementId(1),
                [(PropKey::X, PropValue::Num(1.0))],
                &mut clock,
                10,
            ),
        );

        let clear_ops = clear(&document, &mut clock, 20);

        let mut forward = document.clone();
        apply_all(&mut forward, &clear_ops);

        let mut reversed = document.clone();
        let mut flipped = clear_ops.clone();
        flipped.reverse();
        apply_all(&mut reversed, &flipped);

        assert_eq!(forward, reversed);
    }
}
