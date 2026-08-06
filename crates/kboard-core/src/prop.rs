//! Element properties.
//!
//! Properties are stored in a map rather than as struct fields for two reasons
//! that both come from this engine being *embedded*:
//!
//! 1. Merge is one loop over a map instead of one branch per field, so adding a
//!    property cannot introduce a merge bug.
//! 2. Hosts can attach their own properties via [`PropKey::Custom`] without an
//!    ABI break, which matters when the consumer is a foreign runtime that
//!    links against a compiled artifact.
//!
//! Typed accessors on [`crate::element::Element`] keep the renderer honest.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A point in scene space. `pressure` is only meaningful for freehand strokes.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub pressure: f32,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            pressure: 0.0,
        }
    }

    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.pressure.is_finite()
    }
}

/// What a shape is. Stored as a property so it merges like everything else —
/// converting a rectangle to a diamond is an ordinary concurrent edit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementKind {
    Rectangle,
    Ellipse,
    Diamond,
    Line,
    Arrow,
    Freedraw,
    Text,
    Frame,
    Image,
}

/// Known property keys, plus an escape hatch for host extensions.
///
/// `Ord` matters: properties live in a `BTreeMap`, so iteration order is stable
/// across replicas and platforms. That stability is what makes serialised
/// documents byte-comparable in tests.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PropKey {
    Kind,
    X,
    Y,
    Width,
    Height,
    Angle,
    Stroke,
    Fill,
    StrokeWidth,
    Opacity,
    Roughness,
    Text,
    FontSize,
    FontFamily,
    Points,
    /// Fractional z-order key. See [`crate::frac`].
    ZIndex,
    Locked,
    /// Tombstone. Elements are never removed from the map by merge; removal is
    /// a host-driven garbage-collection decision.
    Deleted,
    GroupId,
    /// Host-defined. The engine stores and merges these but never interprets
    /// them, which keeps host semantics out of the engine.
    Custom(String),
}

impl PropKey {
    /// Stable wire name.
    ///
    /// Serialised as a plain string rather than a derived enum because these
    /// are JSON *map keys*, and JSON map keys must be strings — a derived
    /// newtype variant would encode as an object and fail at runtime on the
    /// first custom property a host attached.
    ///
    /// Custom keys carry a `~` prefix so a host-defined name can never collide
    /// with a known key, including keys added in future versions.
    pub fn as_wire(&self) -> Cow<'_, str> {
        Cow::Borrowed(match self {
            Self::Kind => "kind",
            Self::X => "x",
            Self::Y => "y",
            Self::Width => "w",
            Self::Height => "h",
            Self::Angle => "angle",
            Self::Stroke => "stroke",
            Self::Fill => "fill",
            Self::StrokeWidth => "strokeWidth",
            Self::Opacity => "opacity",
            Self::Roughness => "roughness",
            Self::Text => "text",
            Self::FontSize => "fontSize",
            Self::FontFamily => "fontFamily",
            Self::Points => "points",
            Self::ZIndex => "z",
            Self::Locked => "locked",
            Self::Deleted => "deleted",
            Self::GroupId => "groupId",
            Self::Custom(name) => return Cow::Owned(format!("~{name}")),
        })
    }

    /// Parse a wire name. Unknown names become [`PropKey::Custom`] rather than
    /// an error, so an older replica round-trips a newer one's properties
    /// instead of destroying them.
    pub fn from_wire(text: &str) -> Self {
        match text {
            "kind" => Self::Kind,
            "x" => Self::X,
            "y" => Self::Y,
            "w" => Self::Width,
            "h" => Self::Height,
            "angle" => Self::Angle,
            "stroke" => Self::Stroke,
            "fill" => Self::Fill,
            "strokeWidth" => Self::StrokeWidth,
            "opacity" => Self::Opacity,
            "roughness" => Self::Roughness,
            "text" => Self::Text,
            "fontSize" => Self::FontSize,
            "fontFamily" => Self::FontFamily,
            "points" => Self::Points,
            "z" => Self::ZIndex,
            "locked" => Self::Locked,
            "deleted" => Self::Deleted,
            "groupId" => Self::GroupId,
            other => Self::Custom(other.strip_prefix('~').unwrap_or(other).to_owned()),
        }
    }

    /// Conservative in-memory payload estimate used by hosts for aggregate
    /// room budgets. It intentionally includes key overhead rather than
    /// claiming to be an allocator-exact measurement.
    pub fn estimated_bytes(&self) -> usize {
        match self {
            Self::Custom(name) => 16_usize.saturating_add(name.len()),
            _ => 16,
        }
    }
}

impl Serialize for PropKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_wire())
    }
}

impl<'de> Deserialize<'de> for PropKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_wire(&String::deserialize(deserializer)?))
    }
}

/// A property value.
///
/// Deliberately small and closed. Anything richer belongs in the host's own
/// storage, keyed by element id — putting it here would make the engine care
/// about domain semantics it has no business knowing.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum PropValue {
    Null,
    Bool(bool),
    Num(f64),
    Int(i64),
    Text(String),
    /// Packed RGBA, one byte per channel.
    Color(u32),
    Kind(ElementKind),
    Points(Vec<Point>),
}

impl PropValue {
    /// Reject values that would poison rendering or comparison.
    ///
    /// NaN is the important case: it breaks both ordering and equality, so it
    /// must never enter the document. Hosts should call this at their trust
    /// boundary — the engine also enforces it on write.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Num(n) => n.is_finite(),
            Self::Points(points) => points.iter().all(Point::is_finite),
            Self::Text(text) => text.len() <= MAX_TEXT_BYTES,
            _ => true,
        }
    }

    /// Whether this value is both safe and meaningful for `key`.
    ///
    /// Custom host properties deliberately accept every bounded value. Known
    /// properties are strict: a future-stamped string in `deleted`, for
    /// example, would otherwise dominate the boolean tombstone while readers
    /// interpreted it as `false`.
    pub fn is_valid_for(&self, key: &PropKey) -> bool {
        if !self.is_valid() {
            return false;
        }

        match key {
            PropKey::Kind => matches!(self, Self::Kind(_)),
            PropKey::X
            | PropKey::Y
            | PropKey::Width
            | PropKey::Height
            | PropKey::Angle
            | PropKey::StrokeWidth
            | PropKey::Opacity
            | PropKey::Roughness
            | PropKey::FontSize => matches!(self, Self::Num(_) | Self::Int(_)),
            PropKey::Stroke | PropKey::Fill => matches!(self, Self::Color(_)),
            PropKey::Text | PropKey::FontFamily | PropKey::ZIndex => {
                matches!(self, Self::Text(_))
            }
            PropKey::Points => matches!(self, Self::Points(_)),
            PropKey::Locked | PropKey::Deleted => matches!(self, Self::Bool(_)),
            // Null clears optional group membership without inventing a
            // separate operation; custom keys retain the engine's extension
            // value vocabulary.
            PropKey::GroupId => matches!(self, Self::Text(_) | Self::Null),
            PropKey::Custom(_) => true,
        }
    }

    pub fn as_num(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_kind(&self) -> Option<ElementKind> {
        match self {
            Self::Kind(k) => Some(*k),
            _ => None,
        }
    }

    pub fn as_points(&self) -> Option<&[Point]> {
        match self {
            Self::Points(p) => Some(p),
            _ => None,
        }
    }

    /// Conservative payload size for server resource governance.
    pub fn estimated_bytes(&self) -> usize {
        match self {
            Self::Text(text) => 24_usize.saturating_add(text.len()),
            Self::Points(points) => {
                24_usize.saturating_add(points.len().saturating_mul(std::mem::size_of::<Point>()))
            }
            _ => 24,
        }
    }
}

/// Upper bound on a single text property. Hosts impose their own limits too;
/// this one exists so a malformed peer cannot exhaust memory on every replica.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_is_rejected() {
        assert!(!PropValue::Num(f64::NAN).is_valid());
        assert!(!PropValue::Num(f64::INFINITY).is_valid());
        assert!(PropValue::Num(0.0).is_valid());
    }

    #[test]
    fn non_finite_points_are_rejected() {
        let bad = PropValue::Points(vec![Point::new(0.0, f64::NAN)]);
        assert!(!bad.is_valid());
    }

    #[test]
    fn oversized_text_is_rejected() {
        let big = PropValue::Text("x".repeat(MAX_TEXT_BYTES + 1));
        assert!(!big.is_valid());
    }

    #[test]
    fn known_keys_reject_values_their_readers_cannot_interpret() {
        assert!(PropValue::Num(1.0).is_valid_for(&PropKey::X));
        assert!(PropValue::Int(1).is_valid_for(&PropKey::X));
        assert!(!PropValue::Text("1".into()).is_valid_for(&PropKey::X));
        assert!(PropValue::Bool(true).is_valid_for(&PropKey::Deleted));
        assert!(!PropValue::Text("false".into()).is_valid_for(&PropKey::Deleted));
        assert!(PropValue::Null.is_valid_for(&PropKey::GroupId));
    }

    #[test]
    fn custom_keys_keep_the_bounded_extension_value_vocabulary() {
        let key = PropKey::Custom("host-data".into());
        assert!(PropValue::Null.is_valid_for(&key));
        assert!(PropValue::Text("value".into()).is_valid_for(&key));
        assert!(!PropValue::Num(f64::NAN).is_valid_for(&key));
    }

    #[test]
    fn custom_keys_order_after_known_keys() {
        // Guards the BTreeMap iteration-order guarantee that serialisation
        // stability depends on.
        let mut keys = [PropKey::Custom("a".into()), PropKey::X, PropKey::Kind];
        keys.sort();
        assert_eq!(keys[0], PropKey::Kind);
        assert_eq!(keys[2], PropKey::Custom("a".into()));
    }
}
