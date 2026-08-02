# ADR-0001: Resolve conflicts per property, not per element

- **Status:** Accepted
- **Date:** 2026-08-02
- **Owners:** Engine
- **Related:** [ADR-0002](0002-clear-as-explicit-deletes.md)

## Context

A collaborative canvas must decide what happens when two people edit the same
shape without having seen each other. The choice of granularity for that
decision determines whether the product loses user work.

Excalidraw resolves at the *element* level using `version` and `versionNonce`:
the whole element with the higher version wins. When one person drags a
rectangle while another recolours it, the two edits contend even though they
touched unrelated attributes, and one is discarded. There is no conflict signal
— the user simply finds their work undone, usually without noticing when.

tldraw resolves at record level, with the same class of outcome. Figma resolves
per property and does not have this problem.

k-board's entire justification for existing is that it is embedded in platforms
that own durable, audited content. Silently discarding a write is not an
acceptable failure mode in that setting.

## Decision

Every element is a map of independently convergent last-writer-wins registers,
one per property, each stamped with a hybrid logical clock. A document is an
LWW-map of elements; an element is an LWW-map of properties.

Ordering is total: `(wall, counter, actor)` compared lexicographically. Including
the actor makes concurrent stamps orderable rather than tied, so every replica
selects the same winner without coordinating.

Two people editing *different* properties of one shape never contend. Two people
editing the *same* property resolve deterministically in any arrival order.

Text is currently one such register. Concurrent edits to a single text block
therefore keep only one. This is a known limitation, documented rather than
hidden; see revisit triggers.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Element-level LWW (Excalidraw's model) | Smallest metadata; trivial to implement | Loses one of two concurrent edits to the same shape | The defect this project exists to remove |
| Operational transformation | Mature; used by Google Docs | Requires a central transforming authority | Incompatible with embedding in a host that owns its own transport |
| Full sequence CRDT everywhere (RGA/Yjs-style) | Correct for text as well as structure | Heavier metadata; tombstone growth; unnecessary for a map of shapes | Cost is not justified for element geometry, which is a map not a sequence |
| Server-authoritative ordering | No client-side merge at all | Requires a server; breaks local-first and embedded operation | The engine must work offline and inside foreign hosts |

## Consequences

### Positive

- Concurrent edits to distinct attributes of one shape all survive.
- Merge is commutative, associative, and idempotent, so replicas may gossip in
  any order, over unreliable transports, more than once.
- Undo becomes cheap: writing a prior value with a fresh, higher stamp converges
  like any other edit. No inverse-operation machinery is required.

### Negative and accepted trade-offs

- Metadata cost is one stamp (16 bytes) per property rather than per element.
- Concurrent edits to one text block keep only one.
- Tombstones are retained until a host decides it is safe to collect them.

### Security and privacy consequences

The stamp carries an opaque, host-assigned actor identifier and no other
identity. Nothing in the merge path can be used to attribute content beyond
what the host itself already knows.

## Validation

- `every_ordering_of_a_log_produces_one_document` — all 5,040 orderings of a
  contended seven-operation log yield one document.
- `concurrent_edits_to_distinct_properties_never_lose_data` — 20 actors write 20
  properties of one element concurrently; all 20 survive.
- `merge_is_associative_across_replicas`, `a_three_way_partition_heals_completely`.
- End to end against a running server: a move and a restyle issued concurrently
  by two live replicas both survive.

## Revisit triggers

- Concurrent text editing within a single element becomes a requirement — then a
  sequence CRDT replaces the text register specifically, not the whole model.
- Property metadata is measured as a material share of document size.
- A host requires causal or transactional grouping of multiple property writes.
