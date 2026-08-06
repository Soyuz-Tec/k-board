//! Versioned WebSocket protocol contracts.
//!
//! Version 1 remains frozen through fixtures while version 2 adds stable
//! replica identity, batch identity and durable outcomes. Negotiation happens
//! at the WebSocket subprotocol boundary; messages still carry `version` where
//! it makes diagnostics and fixtures unambiguous.

use kboard_core::clock::ActorId;
use kboard_core::document::Document;
use kboard_core::op::StampedOp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION_2: u16 = 2;
pub const VERSION_2_SUBPROTOCOL: &str = "kboard.v2";
pub const ID_HEX_BYTES: usize = 16;
pub const ID_HEX_LENGTH: usize = ID_HEX_BYTES * 2;
pub const MAX_FUTURE_SKEW_MILLIS: u64 = 5 * 60 * 1_000;
pub const DEDUPE_RETENTION_MILLIS: u64 = 30 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum V1ClientMessage {
    Ops { ops: Vec<StampedOp> },
    Presence { x: f64, y: f64 },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum V1ServerMessage<'a> {
    Init { actor: u64, doc: &'a Document },
    Ops { ops: &'a [StampedOp] },
    Presence { actor: u64, x: f64, y: f64 },
    Left { actor: u64 },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum V2ClientMessage {
    Hello { version: u16, replica: String },
    Ops { batch: String, ops: Vec<StampedOp> },
    Presence { x: f64, y: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    ActorMismatch,
    BatchConflict,
    BatchTooLarge,
    ClockSkew,
    InvalidBatch,
    InvalidOperation,
    NotDurable,
    Overloaded,
    Protocol,
    ReplicaInUse,
    RoomFull,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum V2ServerMessage<'a> {
    Init {
        version: u16,
        actor: u64,
        replica: &'a str,
        doc: &'a Document,
    },
    Ack {
        batch: &'a str,
        sequence: u64,
    },
    Refused {
        batch: Option<&'a str>,
        code: RefusalCode,
        retryable: bool,
    },
    Ops {
        version: u16,
        sequence: u64,
        ops: &'a [StampedOp],
    },
    Presence {
        version: u16,
        actor: u64,
        x: f64,
        y: f64,
    },
    Left {
        version: u16,
        actor: u64,
    },
}

pub fn valid_opaque_id(value: &str) -> bool {
    value.len() == ID_HEX_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub fn operations_hash(ops: &[StampedOp]) -> Result<[u8; 32], serde_json::Error> {
    let payload = serde_json::to_vec(ops)?;
    Ok(Sha256::digest(payload).into())
}

/// Deterministically map a scope-local replica identity into JSON's exact
/// integer range. The full replica id remains the deduplication principal; the
/// actor is the compact CRDT tiebreak and element-id prefix.
pub fn actor_for(scope: &str, replica: &str) -> ActorId {
    let mut digest = Sha256::new();
    digest.update(b"kboard-replica-v2\0");
    digest.update(scope.as_bytes());
    digest.update(b"\0");
    digest.update(replica.as_bytes());
    let bytes = digest.finalize();
    let mut actor = u64::from_be_bytes([
        0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
    ]) & ((1_u64 << 53) - 1);
    if actor == 0 {
        actor = 1;
    }
    ActorId(actor)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StampError {
    ActorMismatch,
    FutureSkew,
}

pub fn validate_stamps(
    ops: &[StampedOp],
    actor: ActorId,
    server_now_millis: u64,
) -> Result<(), StampError> {
    let latest = server_now_millis.saturating_add(MAX_FUTURE_SKEW_MILLIS);
    for stamped in ops {
        if stamped.stamp.actor != actor {
            return Err(StampError::ActorMismatch);
        }
        if stamped.stamp.wall > latest {
            return Err(StampError::FutureSkew);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::{ActorId, Hlc};
    use kboard_core::document::ScopeId;
    use kboard_core::element::ElementId;
    use kboard_core::op::Op;
    use kboard_core::prop::{PropKey, PropValue};
    use serde_json::{json, Value};

    fn operation() -> StampedOp {
        StampedOp::new(
            Hlc {
                wall: 1_000,
                counter: 0,
                actor: ActorId(1),
            },
            Op::Set {
                element: ElementId(1),
                key: PropKey::X,
                value: PropValue::Num(1.0),
            },
        )
    }

    fn fixture(name: &str) -> Value {
        let text = match name {
            "client_ops" => include_str!("../fixtures/protocol/v1/client-ops.json"),
            "client_presence" => include_str!("../fixtures/protocol/v1/client-presence.json"),
            "server_init" => include_str!("../fixtures/protocol/v1/server-init.json"),
            "server_ops" => include_str!("../fixtures/protocol/v1/server-ops.json"),
            "server_presence" => include_str!("../fixtures/protocol/v1/server-presence.json"),
            "server_left" => include_str!("../fixtures/protocol/v1/server-left.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn protocol_v1_json_shapes_are_frozen_as_fixtures() {
        let op = operation();
        let ops = [op.clone()];
        let document = Document::new(ScopeId::new("fixture"));

        assert_eq!(
            serde_json::to_value(V1ClientMessage::Ops { ops: vec![op] }).unwrap(),
            fixture("client_ops")
        );
        assert_eq!(
            serde_json::to_value(V1ClientMessage::Presence { x: 1.5, y: 2.5 }).unwrap(),
            fixture("client_presence")
        );
        assert_eq!(
            serde_json::to_value(V1ServerMessage::Init {
                actor: 1,
                doc: &document
            })
            .unwrap(),
            fixture("server_init")
        );
        assert_eq!(
            serde_json::to_value(V1ServerMessage::Ops { ops: &ops }).unwrap(),
            fixture("server_ops")
        );
        assert_eq!(
            serde_json::to_value(V1ServerMessage::Presence {
                actor: 1,
                x: 1.5,
                y: 2.5
            })
            .unwrap(),
            fixture("server_presence")
        );
        assert_eq!(
            serde_json::to_value(V1ServerMessage::Left { actor: 1 }).unwrap(),
            fixture("server_left")
        );
    }

    #[test]
    fn replica_and_batch_ids_are_canonical_lowercase_hex() {
        assert!(valid_opaque_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_opaque_id("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!valid_opaque_id("short"));
        assert!(!valid_opaque_id("g123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn actor_mapping_is_scope_bound_stable_and_json_exact() {
        let replica = "0123456789abcdef0123456789abcdef";
        let first = actor_for("tenant:board", replica);
        assert_eq!(first, actor_for("tenant:board", replica));
        assert_ne!(first, actor_for("tenant:other", replica));
        assert!(first.0 > 0 && first.0 < (1_u64 << 53));
        assert_eq!(
            serde_json::from_value::<u64>(json!(first.0)).unwrap(),
            first.0
        );
    }

    #[test]
    fn stamps_are_bound_to_the_replica_actor_and_future_window() {
        let actor = ActorId(7);
        let mut op = operation();
        op.stamp.actor = actor;
        op.stamp.wall = 10_000;
        assert_eq!(validate_stamps(&[op.clone()], actor, 10_000), Ok(()));

        op.stamp.actor = ActorId(8);
        assert_eq!(
            validate_stamps(&[op.clone()], actor, 10_000),
            Err(StampError::ActorMismatch)
        );
        op.stamp.actor = actor;
        op.stamp.wall = 10_000 + MAX_FUTURE_SKEW_MILLIS + 1;
        assert_eq!(
            validate_stamps(&[op], actor, 10_000),
            Err(StampError::FutureSkew)
        );
    }

    #[test]
    fn canonical_operation_hash_changes_with_the_payload() {
        let first = operation();
        let mut changed = first.clone();
        changed.stamp.counter = 1;

        assert_eq!(
            operations_hash(std::slice::from_ref(&first)).unwrap(),
            operations_hash(&[first]).unwrap()
        );
        assert_ne!(
            operations_hash(&[changed]).unwrap(),
            operations_hash(&[operation()]).unwrap()
        );
    }

    #[test]
    fn additive_fields_are_ignored_but_unknown_message_types_are_rejected() {
        let with_future_field = r#"{"type":"hello","version":2,"replica":"0123456789abcdef0123456789abcdef","future":true}"#;
        assert!(matches!(
            serde_json::from_str::<V2ClientMessage>(with_future_field),
            Ok(V2ClientMessage::Hello { .. })
        ));
        assert!(serde_json::from_str::<V2ClientMessage>(r#"{"type":"future"}"#).is_err());
    }
}
