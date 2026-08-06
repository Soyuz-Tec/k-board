//! The element: a bag of independently convergent properties.
//!
//! The important design choice here is that the unit of conflict resolution is
//! the *property*, not the element.
//!
//! Editors that merge whole elements — Excalidraw's `version`/`versionNonce`
//! rule is the well-known example — lose one of two concurrent edits whenever
//! two people touch the same shape, even when they touched different things.
//! One person drags a rectangle while another recolours it, and one of those
//! edits silently disappears. Merging per property makes both survive, because
//! they never contend for the same register.

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::clock::ActorId;
use crate::clock::Hlc;
use crate::lww::Lww;
use crate::prop::{ElementKind, Point, PropKey, PropValue};

/// Host-assigned element identity.
///
/// Serialised as a 32-character hex string rather than a number: JSON cannot
/// carry 128 bits of integer precision, and every host language can compare
/// hex strings.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ElementId(pub u128);

impl ElementId {
    pub fn to_hex(self) -> String {
        format!("{:032x}", self.0)
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        u128::from_str_radix(text.trim_start_matches("0x"), 16)
            .ok()
            .map(Self)
    }

    /// Actor that originally minted this id.
    pub const fn actor(self) -> ActorId {
        ActorId((self.0 >> 64) as u64)
    }

    /// Actor-local monotonic portion of this id.
    pub const fn local_counter(self) -> u64 {
        self.0 as u64
    }
}

impl Serialize for ElementId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ElementId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text).ok_or_else(|| D::Error::custom("element id must be hex"))
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Element {
    id: ElementId,
    props: BTreeMap<PropKey, Lww<PropValue>>,
}

impl Element {
    pub fn new(id: ElementId) -> Self {
        Self {
            id,
            props: BTreeMap::new(),
        }
    }

    pub const fn id(&self) -> ElementId {
        self.id
    }

    pub fn get(&self, key: &PropKey) -> Option<&PropValue> {
        self.props.get(key).map(Lww::get)
    }

    /// Write a property locally.
    ///
    /// Returns `false` for a stale stamp or an invalid value. Both are ordinary
    /// outcomes, not errors: a stale write losing is the convergent result, and
    /// rejecting NaN keeps it out of every downstream replica.
    pub fn set(&mut self, key: PropKey, value: PropValue, stamp: Hlc) -> bool {
        if !value.is_valid_for(&key) {
            return false;
        }
        match self.props.get_mut(&key) {
            Some(register) => register.set(value, stamp),
            None => {
                self.props.insert(key, Lww::new(value, stamp));
                true
            }
        }
    }

    /// Merge another replica's view of this element.
    ///
    /// Union of property maps, last-writer-wins per property. Commutative,
    /// associative, and idempotent because each register has those properties
    /// and a union of such maps preserves them.
    pub fn merge(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (key, incoming) in &other.props {
            if !incoming.get().is_valid_for(key) {
                continue;
            }
            match self.props.get_mut(key) {
                Some(current) => changed |= current.merge(incoming),
                None => {
                    self.props.insert(key.clone(), incoming.clone());
                    changed = true;
                }
            }
        }
        changed
    }

    /// Tombstoned elements stay in the document so that a late-arriving edit
    /// cannot resurrect them by being merged into an absent entry.
    pub fn is_deleted(&self) -> bool {
        self.get(&PropKey::Deleted)
            .and_then(PropValue::as_bool)
            .unwrap_or(false)
    }

    /// Highest stamp across all properties. Used to decide what a snapshot has
    /// already absorbed, and what a tombstone sweep may collect.
    pub fn max_stamp(&self) -> Option<Hlc> {
        self.props.values().map(Lww::stamp).max()
    }

    pub fn props(&self) -> impl Iterator<Item = (&PropKey, &PropValue)> {
        self.props
            .iter()
            .map(|(key, register)| (key, register.get()))
    }

    pub fn prop_count(&self) -> usize {
        self.props.len()
    }

    // -- typed accessors ---------------------------------------------------
    // The renderer should never pattern-match PropValue by hand.

    pub fn kind(&self) -> Option<ElementKind> {
        self.get(&PropKey::Kind).and_then(PropValue::as_kind)
    }

    pub fn num(&self, key: PropKey) -> Option<f64> {
        self.get(&key).and_then(PropValue::as_num)
    }

    pub fn num_or(&self, key: PropKey, fallback: f64) -> f64 {
        self.num(key).unwrap_or(fallback)
    }

    pub fn x(&self) -> f64 {
        self.num_or(PropKey::X, 0.0)
    }

    pub fn y(&self) -> f64 {
        self.num_or(PropKey::Y, 0.0)
    }

    pub fn width(&self) -> f64 {
        self.num_or(PropKey::Width, 0.0)
    }

    pub fn height(&self) -> f64 {
        self.num_or(PropKey::Height, 0.0)
    }

    pub fn text(&self) -> Option<&str> {
        self.get(&PropKey::Text).and_then(PropValue::as_text)
    }

    pub fn points(&self) -> Option<&[Point]> {
        self.get(&PropKey::Points).and_then(PropValue::as_points)
    }

    /// Fractional z-order key. Elements without one sort to the bottom.
    pub fn z_index(&self) -> &str {
        self.get(&PropKey::ZIndex)
            .and_then(PropValue::as_text)
            .unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ActorId;

    const A: ActorId = ActorId(1);
    const B: ActorId = ActorId(2);

    fn stamp(wall: u64, actor: ActorId) -> Hlc {
        Hlc {
            wall,
            counter: 0,
            actor,
        }
    }

    fn moved(id: ElementId, x: f64, at: Hlc) -> Element {
        let mut element = Element::new(id);
        element.set(PropKey::X, PropValue::Num(x), at);
        element
    }

    #[test]
    fn concurrent_edits_to_different_properties_both_survive() {
        // The defect this whole engine exists to remove: one person moves a
        // shape while another recolours it.
        let id = ElementId(7);

        let mut dragged = Element::new(id);
        dragged.set(PropKey::X, PropValue::Num(120.0), stamp(10, A));

        let mut recoloured = Element::new(id);
        recoloured.set(PropKey::Stroke, PropValue::Color(0xFF0000FF), stamp(10, B));

        dragged.merge(&recoloured);

        assert_eq!(dragged.num(PropKey::X), Some(120.0), "the move survived");
        assert_eq!(
            dragged.get(&PropKey::Stroke),
            Some(&PropValue::Color(0xFF0000FF)),
            "the recolour survived"
        );
    }

    #[test]
    fn concurrent_edits_to_the_same_property_resolve_deterministically() {
        let id = ElementId(7);
        let mut from_a = moved(id, 10.0, stamp(5, A));
        let from_b = moved(id, 20.0, stamp(5, B));

        let mut reversed = from_b.clone();
        reversed.merge(&from_a);
        from_a.merge(&from_b);

        assert_eq!(from_a.num(PropKey::X), reversed.num(PropKey::X));
    }

    #[test]
    fn merge_is_idempotent() {
        let id = ElementId(1);
        let mut left = moved(id, 1.0, stamp(1, A));
        let right = moved(id, 2.0, stamp(2, B));

        left.merge(&right);
        let after_first = left.clone();
        left.merge(&right);

        assert_eq!(left, after_first);
    }

    #[test]
    fn invalid_values_never_enter_the_document() {
        let mut element = Element::new(ElementId(1));
        assert!(!element.set(PropKey::X, PropValue::Num(f64::NAN), stamp(1, A)));
        assert_eq!(element.get(&PropKey::X), None);
    }

    #[test]
    fn incompatible_known_property_values_never_dominate_valid_ones() {
        let mut element = Element::new(ElementId(1));
        assert!(element.set(PropKey::Deleted, PropValue::Bool(true), stamp(1, A)));
        assert!(!element.set(
            PropKey::Deleted,
            PropValue::Text("false".into()),
            stamp(2, B)
        ));
        assert!(element.is_deleted());
    }

    #[test]
    fn merge_ignores_an_incompatible_future_value() {
        let mut valid = Element::new(ElementId(1));
        valid.props.insert(
            PropKey::Deleted,
            Lww::new(PropValue::Bool(true), stamp(1, A)),
        );
        let mut malformed = Element::new(ElementId(1));
        malformed.props.insert(
            PropKey::Deleted,
            Lww::new(PropValue::Text("false".into()), stamp(2, B)),
        );

        assert!(!valid.merge(&malformed));
        assert!(valid.is_deleted());
    }

    #[test]
    fn stale_writes_are_dropped() {
        let mut element = Element::new(ElementId(1));
        element.set(PropKey::X, PropValue::Num(100.0), stamp(10, A));
        assert!(!element.set(PropKey::X, PropValue::Num(1.0), stamp(2, A)));
        assert_eq!(element.num(PropKey::X), Some(100.0));
    }

    #[test]
    fn element_id_survives_a_hex_round_trip() {
        let id = ElementId(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(ElementId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(id.to_hex().len(), 32);
    }

    #[test]
    fn element_id_exposes_its_actor_and_local_counter() {
        let id = ElementId((u128::from(9_u64) << 64) | 42);
        assert_eq!(id.actor(), ActorId(9));
        assert_eq!(id.local_counter(), 42);
    }

    #[test]
    fn custom_host_properties_merge_like_any_other() {
        let id = ElementId(3);
        let key = PropKey::Custom("kcomms:author".into());

        let mut left = Element::new(id);
        left.set(key.clone(), PropValue::Text("alice".into()), stamp(1, A));
        let mut right = Element::new(id);
        right.set(key.clone(), PropValue::Text("bob".into()), stamp(2, B));

        left.merge(&right);
        assert_eq!(left.get(&key).and_then(PropValue::as_text), Some("bob"));
    }
}
