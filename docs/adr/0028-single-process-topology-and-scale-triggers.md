# ADR-0028: Keep one authoritative process until placement and fencing exist

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Architecture, Server, Operations
- **Related:** [ADR-0015](0015-per-scope-room-cells.md), [ADR-0022](0022-bounded-fair-storage-writer.md), [ADR-0027](0027-production-lifecycle-and-recovery.md)

## Context

Per-scope cells remove in-process cross-scope ownership contention but do not
make two server processes safe. Shared SQLite neither places a scope nor fences
an old owner. Sticky routing can reduce accidental overlap but cannot prevent
split brain after a proxy, process or network failure. Extracting each source
module into a service would add failure modes without resolving ownership.

The measured debug-build workload qualified 64 simultaneous active scopes at
31.25 ms cold-scope p99. At 96 simultaneous first-wave writers, the bounded
64-entry storage queue returned explicit overload. This is a qualification
envelope on one Windows workstation, not the product maximum.

## Decision

One `kboard-server` process is the only supported authoritative writer topology.
Two active writers for one scope are unsupported even if they share a database.
Scale vertically, tune admitted concurrency from production evidence, and use
drain/restart deployment. No module becomes a network service merely because it
is a source boundary.

Any horizontal design requires a superseding ADR and all of:

1. a canonical placement key equal to the opaque scope id;
2. one leased owner per scope in a strongly consistent coordinator;
3. monotonically increasing fencing tokens checked by every durable append;
4. join/commit refusal by an owner whose lease or fence is stale;
5. handoff that stops admission, drains the old cell, commits its terminal
   sequence, transfers the fence, then restores the new cell;
6. presence routed through the current owner or an explicitly ephemeral
   cross-process bus, never the durable CRDT log;
7. a database that supports the chosen concurrent writers and atomic fence
   check (SQLite on a shared filesystem is not that database);
8. placement, lease expiry, split-brain, coordinator outage, clock and rolling
   deployment fault tests before production.

Sticky routing may remain an optimization but not correctness. Leased ownership
plus storage fencing is the required correctness mechanism.

## ATAM review

| Scenario | One process | Sticky routing only | Lease plus fencing |
|---|---|---|---|
| Process loss | All scopes reconnect/restore; bounded RTO | Routes can move, old owner may still write | Lease expiry and new fence permit safe reassignment |
| Network partition | No internal split brain | Proxy views can create two owners | Stale fence is rejected at durable append |
| Hot scope | One cell/mailbox contains it | Can place it, no ownership proof | Can place it with operational coordinator cost |
| Deployment | Bounded process drain/restart | Overlap risks dual writers | Per-scope handoff is possible |
| Presence | In-process broadcast | Cross-process peers may disagree | Owner/bus fan-out is explicit |
| Operational complexity | Lowest | Medium but unsafe | Highest; coordinator, leases, fencing and distributed database |

The current demand and 64-scope qualified envelope do not justify the third
column's added availability and consistency mechanisms. Overload at 96 is a
capacity signal to tune/test; sustained production approach to the envelope is
the trigger to run a new ATAM, not permission to add an unfenced replica.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Shared SQLite plus two servers | Easy-looking scale | No scope ownership or fencing; unsafe filesystem semantics | Split brain |
| Sticky routing | Minimal infrastructure | Affinity is not a consistency protocol | Optimization only |
| Lease, fencing and multi-writer database now | Safe basis for horizontal scale | Large operational and failure-mode cost before demand | Premature |
| One authoritative modular monolith | Matches ownership model and measured demand | Process is one failure/scale domain | **Chosen** |

## Consequences

- Deployment uses bounded drain and restart; it does not overlap writers.
- The storage queue is the current measured scale boundary and stays bounded.
- A topology change is incomplete without a superseding ADR and fault evidence.
- Source modules remain replaceable ports without becoming remote services.

## Validation and revisit triggers

Run `node scripts/single-process-capacity-check.mjs` on production-like hardware
and retain the raw result. Revisit when sustained p99 exceeds the objective,
storage overload occurs in legitimate traffic, one process cannot meet memory
or connection demand, or fault-domain requirements exceed the published RTO.
