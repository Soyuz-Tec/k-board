# ADR-0002: Expand "clear the board" into explicit deletes at the origin

- **Status:** Accepted
- **Date:** 2026-08-02
- **Owners:** Engine
- **Related:** [ADR-0001](0001-per-property-convergence.md)

## Context

"Clear the board" is a single user gesture with board-wide effect. Every other
operation in the engine names one element, so clear is the only candidate for a
scope-wide operation type.

Modelling it as its own operation looks obviously right and is wrong.

Each replica would expand a received `Clear` against whatever elements *it*
currently knows about. A replica that has not yet received element X would not
tombstone it; a replica that has would. Both then continue, permanently
disagreeing about whether X exists. No amount of re-gossiping repairs it,
because each replica's state is internally consistent with the operations it
applied. This is exactly the failure a CRDT is supposed to make impossible, and
introducing one board-wide operation would reintroduce it.

## Decision

There is no `Clear` operation on the wire or in the log.

`op::clear` is a client-side helper that expands the gesture into an explicit
`Delete` per currently-live element, stamped at the originating replica. What
enters the log is an ordinary, deterministic set of deletes.

An element the origin had not yet received is not deleted. It survives the
clear on every replica — which is both convergent and what a user expects: you
cannot erase something that had not reached you.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| A `Clear` operation expanded per replica | One small operation on the wire | Replicas with different knowledge produce different tombstone sets | Permanent divergence — disqualifying |
| A board-level generation counter | Compact; one integer | Concurrent edits across a generation boundary must be discarded or resurrected, and either choice loses data | Reintroduces the lost-update problem ADR-0001 removes |
| Server-authoritative clear | Deterministic by fiat | Requires a server; breaks offline and embedded operation | The engine must converge without one |

## Consequences

### Positive

- Clear is not a special case in merge, replay, or snapshotting. It is deletes.
- Convergence proofs cover it without a separate argument.

### Negative and accepted trade-offs

- A clear on a large board emits one operation per live element rather than one
  operation total. On a 10,000-element board that is 10,000 deletes.
- An element in flight during a clear survives it. This is correct but can look
  surprising, and hosts may wish to surface it in the UI.

## Validation

- `clear_does_not_erase_elements_the_origin_never_saw`
- `clear_converges_regardless_of_arrival_order`
- `clear_removes_everything_this_replica_can_see`

## Revisit triggers

- Boards grow large enough that clear-as-N-deletes is a material write burst.
  A range or predicate delete would then need its own convergence argument.
- A host requires "clear including anything in flight", which needs a barrier
  the current model deliberately does not have.
