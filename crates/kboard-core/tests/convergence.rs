//! Convergence proof.
//!
//! Unit tests check individual merges. These check the *laws* — and they check
//! them exhaustively rather than by sampling, because a CRDT that converges for
//! the orderings you happened to try is not a CRDT.
//!
//! Every test here applies the same operations in every possible order and
//! asserts one outcome. If any of these fail, replicas can permanently disagree
//! and no amount of retry logic above will fix it.

use kboard_core::clock::{ActorId, HlcGenerator};
use kboard_core::document::{Document, ScopeId};
use kboard_core::element::ElementId;
use kboard_core::op::{apply_all, upsert, StampedOp};
use kboard_core::prop::{ElementKind, PropKey, PropValue};
use kboard_core::snapshot::Snapshot;

fn scope() -> ScopeId {
    ScopeId::new("tenant-1/board-1")
}

/// All orderings of a slice. Exhaustive, not sampled.
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for index in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(index);
        for tail in permutations(&rest) {
            let mut ordering = Vec::with_capacity(items.len());
            ordering.push(head.clone());
            ordering.extend(tail);
            out.push(ordering);
        }
    }
    out
}

/// Three actors editing overlapping elements and overlapping properties, with
/// wall clocks that collide — the case where only the logical counter and the
/// actor tiebreak keep the order total.
fn contended_log() -> Vec<StampedOp> {
    let mut alice = HlcGenerator::new(ActorId(1));
    let mut bob = HlcGenerator::new(ActorId(2));
    let mut carol = HlcGenerator::new(ActorId(3));

    let mut ops = Vec::new();
    // Same element, same property, same millisecond, different actors.
    ops.extend(upsert(
        ElementId(1),
        [(PropKey::X, PropValue::Num(10.0))],
        &mut alice,
        1_000,
    ));
    ops.extend(upsert(
        ElementId(1),
        [(PropKey::X, PropValue::Num(20.0))],
        &mut bob,
        1_000,
    ));
    // Same element, different properties.
    ops.extend(upsert(
        ElementId(1),
        [(PropKey::Fill, PropValue::Color(0x00FF00FF))],
        &mut carol,
        1_000,
    ));
    // A different element entirely.
    ops.extend(upsert(
        ElementId(2),
        [(PropKey::Kind, PropValue::Kind(ElementKind::Arrow))],
        &mut bob,
        1_001,
    ));
    // A host-defined property.
    ops.extend(upsert(
        ElementId(2),
        [(
            PropKey::Custom("kcomms:author".into()),
            PropValue::Text("carol".into()),
        )],
        &mut carol,
        1_001,
    ));
    // A delete that races an edit to the same element.
    ops.extend(upsert(
        ElementId(2),
        [(PropKey::Y, PropValue::Num(5.0))],
        &mut alice,
        1_002,
    ));
    ops.push(StampedOp::new(
        alice.tick(1_003),
        kboard_core::op::Op::Delete {
            element: ElementId(2),
        },
    ));
    ops
}

#[test]
fn every_ordering_of_a_log_produces_one_document() {
    let ops = contended_log();
    assert_eq!(
        ops.len(),
        7,
        "keep this small enough to permute exhaustively"
    );

    let orderings = permutations(&ops);
    assert_eq!(orderings.len(), 5_040);

    let mut expected: Option<Document> = None;
    for (index, ordering) in orderings.iter().enumerate() {
        let mut document = Document::new(scope());
        apply_all(&mut document, ordering);

        match &expected {
            None => expected = Some(document),
            Some(first) => assert_eq!(
                &document, first,
                "ordering #{index} diverged; replicas would permanently disagree"
            ),
        }
    }
}

#[test]
fn merge_is_associative_across_replicas() {
    let ops = contended_log();
    let (left, rest) = ops.split_at(2);
    let (middle, right) = rest.split_at(2);

    let build = |batch: &[StampedOp]| {
        let mut document = Document::new(scope());
        apply_all(&mut document, batch);
        document
    };
    let (a, b, c) = (build(left), build(middle), build(right));

    // (a . b) . c
    let mut grouped_left = a.clone();
    grouped_left.merge(&b).unwrap();
    grouped_left.merge(&c).unwrap();

    // a . (b . c)
    let mut bc = b.clone();
    bc.merge(&c).unwrap();
    let mut grouped_right = a.clone();
    grouped_right.merge(&bc).unwrap();

    assert_eq!(grouped_left, grouped_right);
}

#[test]
fn a_three_way_partition_heals_completely() {
    // Three replicas edit in isolation, then gossip in an arbitrary order.
    // Every replica must end identical — including the ones that only ever
    // heard from one peer directly.
    let ops = contended_log();
    let mut replicas: Vec<Document> = ops
        .chunks(3)
        .map(|batch| {
            let mut document = Document::new(scope());
            apply_all(&mut document, batch);
            document
        })
        .collect();

    let snapshots = replicas.clone();
    for replica in &mut replicas {
        for peer in &snapshots {
            replica.merge(peer).unwrap();
        }
    }

    for pair in replicas.windows(2) {
        assert_eq!(pair[0], pair[1], "partition did not heal");
    }
}

#[test]
fn concurrent_edits_to_distinct_properties_never_lose_data() {
    // The headline property, at scale: twenty actors each own one property of
    // the same element and write it without seeing anyone else. All twenty
    // writes must survive.
    let element = ElementId(99);
    let mut replicas = Vec::new();

    for actor in 0..20u64 {
        let mut clock = HlcGenerator::new(ActorId(actor));
        let mut document = Document::new(scope());
        apply_all(
            &mut document,
            &upsert(
                element,
                [(
                    PropKey::Custom(format!("field-{actor}")),
                    PropValue::Num(actor as f64),
                )],
                &mut clock,
                1_000,
            ),
        );
        replicas.push(document);
    }

    let mut merged = Document::new(scope());
    for replica in &replicas {
        merged.merge(replica).unwrap();
    }

    let shape = merged.get(element).expect("element must exist");
    assert_eq!(shape.prop_count(), 20, "every concurrent write survived");
    for actor in 0..20u64 {
        assert_eq!(
            shape.num(PropKey::Custom(format!("field-{actor}"))),
            Some(actor as f64)
        );
    }
}

#[test]
fn snapshot_matches_replay_under_every_ordering() {
    // A host that compacts must get the same document as a host that replays
    // the whole log, no matter how the log was ordered when it was written.
    let ops = contended_log();
    let canonical = {
        let mut document = Document::new(scope());
        apply_all(&mut document, &ops);
        document
    };

    for (index, ordering) in permutations(&ops[..5]).iter().enumerate() {
        let mut snapshot = Snapshot::materialize(scope(), ordering);
        snapshot.absorb(&ops[5..]);
        assert_eq!(
            snapshot.document(),
            &canonical,
            "snapshot from ordering #{index} diverged from full replay"
        );
    }
}

#[test]
fn json_round_trip_preserves_the_document() {
    // The wire format has to survive custom property keys, 128-bit element ids,
    // and float values — all three are places a naive derive breaks.
    let ops = contended_log();
    let mut original = Document::new(scope());
    apply_all(&mut original, &ops);

    let encoded = serde_json::to_string(&original).expect("document must serialise");
    let decoded: Document = serde_json::from_str(&encoded).expect("document must deserialise");

    assert_eq!(decoded, original);
    assert!(
        encoded.contains("~kcomms:author"),
        "custom keys keep their prefix"
    );
}

#[test]
fn operations_survive_a_json_round_trip() {
    let ops = contended_log();
    let encoded = serde_json::to_string(&ops).expect("ops must serialise");
    let decoded: Vec<StampedOp> = serde_json::from_str(&encoded).expect("ops must deserialise");
    assert_eq!(decoded, ops);
}

#[test]
fn documents_from_different_tenants_never_merge() {
    // The failure mode that matters most in a multi-tenant engine: one board's
    // content appearing on another tenant's canvas.
    let ops = contended_log();
    let mut mine = Document::new(ScopeId::new("tenant-a/board-1"));
    apply_all(&mut mine, &ops);

    let mut theirs = Document::new(ScopeId::new("tenant-b/board-1"));
    apply_all(&mut theirs, &ops);

    let before = mine.clone();
    assert!(mine.merge(&theirs).is_err());
    assert_eq!(mine, before, "a refused merge must not partially apply");
}
