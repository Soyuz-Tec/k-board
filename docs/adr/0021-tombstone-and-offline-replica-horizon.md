# ADR-0021: Retain tombstones until causal safety is provable

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Engine, Storage, Client, Operations
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0016](0016-durable-batch-acknowledgement.md), [ADR-0018](0018-replica-identity-and-hlc-trust.md)

## Context

Deleting a CRDT tombstone on elapsed wall time alone can resurrect an element
when an offline replica later sends an older update. The standalone protocol
retains successful batch outcomes for 30 days, but that delivery horizon does
not prove every replica has observed every delete. Database growth still needs
an explicit, testable policy.

## Decision

The standalone product does not automatically collect document tombstones.
They remain in snapshots indefinitely until the system has durable per-replica
observation watermarks or another proof that no admissible offline operation can
predate the deletion. `Document::collect_tombstones` remains a host-controlled
primitive, not a timer-driven server action.

Protocol batch outcomes are retained for 30 days. A client offline beyond that
horizon must first resynchronize and may not assume an expired batch identity
still has a recorded outcome. Snapshot transactions bound operation-log growth;
retained tombstones are reported separately so operators can measure their cost.

## Consequences

- Offline convergence is not traded for an arbitrary storage target.
- Snapshot size can grow with deletion history and must be monitored.
- A future garbage collector requires its own ADR, replica-watermark model,
  adversarial offline tests and rollback plan.
- Hosts with stronger causal knowledge may invoke collection under their own
  documented policy without changing the engine.

## Validation

- Log compaction tests prove snapshot plus tail remains exact.
- Tombstones remain present across snapshot, truncation and restore.
- Batch-outcome retention tests remove expired outcomes without removing CRDT
  tombstones or live operations.

## Revisit triggers

- Persistent replica watermarks are available.
- A bounded maximum-offline contract is enforced end to end.
- Snapshot growth exceeds an operational threshold that cannot be addressed by
  compression or archival.
