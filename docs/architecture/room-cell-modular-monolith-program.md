# Per-scope room-cell modular-monolith program

- **Status:** Active
- **Target:** Per-scope room-cell modular monolith
- **Authority:** [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md)
- **Execution rule:** Work in task-ID order inside each gate. A gate closes only
  when its exit evidence is checked in or linked from the change.

## Outcome

Replace server-wide and FFI-wide serialization with explicit per-scope/per-board
ownership while strengthening — not weakening — convergence, durability,
security, recovery, and client delivery truth.

## Audit of the original backlog

The original task list correctly identified room isolation, bounded queues,
lifecycle, observability, performance and FFI contention. The repository audit
found the following missing or under-specified prerequisites.

| Gap found | Why it blocks safe execution | Added work |
|---|---|---|
| No application acknowledgement or batch identity | WebSocket `send` is not durable success; retry cannot be proven safe | G2 acknowledgement, idempotency and compatibility tasks |
| Volatile outbox is cleared on socket send | Close/reload can lose retry intent | G2/G7 ack-retained persistent outbox tasks |
| Operations can forge actor/HLC values | Actor and future timestamps control IDs and LWW winners | G2 replica identity and clock-integrity ADR/tasks |
| Process-local actor allocator resets | Reuse can collide with offline replicas and element IDs | G2 stable replica identity tasks |
| Validation and mutation are one operation | Persistence cannot precede mutation without revalidating | G1 prepared-batch split and atomic refusal tests |
| Room capacity checks only current total | One batch can exceed the element cap | G1 projected-cardinality validation |
| Invalid property values are still logged/relayed | Core silently ignores them while peers receive them | G1 semantic validation and negative tests |
| Valid values can have incompatible known property keys | A future-stamped wrong type can dominate geometry or tombstone state | G1 key/value compatibility validation |
| Append follows in-memory mutation | Failed persistence leaves an unrecoverable process-only edit | G3 append-before-apply transition |
| Snapshot coverage is queried at write time | Async snapshot can claim later operations it does not contain | G3 exact covered-sequence contract |
| Restore failure creates an empty room | Corruption/outage can masquerade as data loss | G3 fail-closed recovery state |
| One SQLite connection remains globally serial | Cells alone do not guarantee cross-scope latency isolation | G3 fair storage scheduling and measurement |
| Cold restores can race | Multiple joins can duplicate restore/work or observe partial state | G4/G5 coalesced restoring state |
| Mailbox cancellation/overload was unspecified | Queued commands can hang or be silently lost | G4 typed result, deadline and overload behavior |
| Async snapshot capture cost was omitted | Serialization may move off-lock while cloning still stalls a cell | G3/G8 capture/serialize/write timings and strategy |
| FFI lock holds during board work/serialization | Independent host boards block one another | G8 registry-to-per-board lock split |
| WebSocket Origin is not checked | Browser cross-site connection policy is incomplete | G6 origin policy and tests |
| Bearer remains in page query | History/referrer/screenshot exposure persists after handshake | G6 token removal/transport tasks |
| Auth is handshake-only | Long sessions can outlive authorization policy | G6 expiry/revalidation decision |
| Only per-connection quotas exist | Many connections can exhaust one scope or identity | G6 aggregate budget tasks |
| Health is liveness-only | Orchestration cannot distinguish unable-to-serve | G9 readiness/degraded-state tasks |
| Shutdown has no room/storage drain contract | Accepted work can be interrupted ambiguously | G9 graceful drain tasks |
| Migrations, disk-full, WAL, backup and restore are incomplete | Operational durability is not demonstrated | G9 database operations tasks |
| Tombstone collection horizon is undecided | Offline replicas and storage growth conflict | G3 ADR and retention tasks |
| Protocol version/mixed-client rollout is absent | Ack/identity changes could strand old clients | G2 version negotiation tasks |
| Raw scope labels would create metric cardinality/leakage | Observability could become a new availability/privacy risk | G5/G8 safe correlation rules |
| Horizontal placement was not bounded | “One cell per scope” is ambiguous across processes | G10 explicit single-writer topology decision |

## Target invariants

1. Exactly one ordered mutation owner exists per active scope in one process.
2. Different scopes do not share mutable document locks or cell mailboxes.
3. Every command has a bounded queue cost and a typed terminal outcome.
4. Batch validation is whole, pure with respect to room state, and precedes I/O.
5. Durable append precedes apply, ack and broadcast.
6. A `(scope, replica, batch)` retry is idempotent.
7. Snapshot document and covered log sequence are captured atomically.
8. Restore errors are explicit unavailable/corrupt states, never empty state.
9. Client outbox entries survive reconnect/reload until durable ack.
10. Unrelated FFI board handles do not share an execution lock.

## Gate 0 — Authority, baseline and decision control

- [x] **G0-01** Confirm repository root, branch, origin and clean starting state.
- [x] **G0-02** Confirm the active local runtime and executable identity.
- [x] **G0-03** Run baseline format, clippy and full workspace tests.
- [x] **G0-04** Inventory crate/module dependency direction.
- [x] **G0-05** Trace WebSocket command, restore, snapshot and broadcast flows.
- [x] **G0-06** Trace browser pending/outbox/reconnect behavior.
- [x] **G0-07** Trace actor allocation, element IDs and HLC trust boundaries.
- [x] **G0-08** Trace SQLite transaction, sequence, snapshot and truncation behavior.
- [x] **G0-09** Trace FFI registry lock duration and output-buffer ownership.
- [x] **G0-10** Record the gap audit before runtime refactoring.
- [x] **G0-11** Establish `docs/ARCHITECTURE.md` as source of truth.
- [x] **G0-12** Add repository working agreements and invariants.
- [x] **G0-13** Accept ADR-0015 for per-scope room cells.
- [x] **G0-14** Accept ADR-0016 for durable acknowledgement and retry.
- [x] **G0-15** Accept ADR-0017 for exact snapshot coverage.
- [x] **G0-16** Record replica identity and clock trust as ADR-0018 Proposed with explicit acceptance evidence.
- [x] **G0-17** Add an automated architecture-document/index check.
- [x] **G0-18** Audit the checked-in performance baseline and record its missing concurrency, memory and DB-size measures for Gate 8.

**Exit evidence:** authority document, indexed controlling decisions (accepted or
proposed with acceptance conditions), passing architecture check, and an audited
reproducible pre-change baseline with missing measures routed to Gate 8.

## Gate 1 — Pure validation and atomic command semantics

- [x] **G1-01** Define a typed `BatchRefusal` taxonomy.
- [x] **G1-02** Separate whole-batch validation from mutation.
- [x] **G1-03** Represent successful validation as a prepared batch.
- [x] **G1-04** Make prepared-batch application infallible under one cell owner.
- [x] **G1-05** Reject too many operations before inspecting the batch body.
- [x] **G1-06** Calculate distinct newly materialized element IDs in a batch.
- [x] **G1-07** Reject projected room-cap overshoot atomically.
- [x] **G1-08** Reject non-finite geometry and numbers before persistence.
- [x] **G1-09** Reject over-limit text before persistence.
- [x] **G1-10** Accept stale/idempotent operations as valid no-ops; deduplicate retries at batch level.
- [x] **G1-11** Verify a refused batch changes no document property.
- [x] **G1-12** Verify a refused batch changes no counters or snapshot trigger.
- [x] **G1-13** Verify an invalid operation makes the whole mixed batch fail.
- [x] **G1-14** Model-test generated batch shapes for validation/application agreement.
- [x] **G1-15** Mutation-corpus test malformed JSON and semantic operation values in ordinary CI.
- [x] **G1-16** Document error-to-protocol mapping without sensitive details.
- [x] **G1-17** Define and test empty operation batches as ignored no-ops.
- [x] **G1-18** Reject known property key/value type mismatches before persistence and in core defence-in-depth.

**Exit evidence:** unit/property tests prove atomic refusal and projected resource
limits without storage or transport involvement.

## Gate 2 — Versioned delivery, acknowledgement, identity and clock integrity

- [x] **G2-01** Inventory current JSON shapes as protocol version 1 fixtures.
- [x] **G2-02** Define compatibility policy and supported-version window.
- [x] **G2-03** Add handshake capability/version negotiation.
- [x] **G2-04** Define stable client-generated or server-issued replica identity.
- [x] **G2-05** Define replica persistence, rotation and logout semantics.
- [x] **G2-06** Bind authorized replica identity to operation actor fields.
- [x] **G2-07** Define element-ID collision handling across restart/reconnect.
- [x] **G2-08** Define server behavior for future-skewed HLC wall values.
- [x] **G2-09** Define server clock observation/normalization behavior.
- [x] **G2-10** Add opaque client batch IDs.
- [x] **G2-11** Define deduplication key and retention horizon.
- [x] **G2-12** Add `Ack(batch_id, durable_sequence)`.
- [x] **G2-13** Add typed refusal/overload outcomes safe for clients.
- [x] **G2-14** Specify ack ordering relative to sender/peer broadcasts.
- [x] **G2-15** Specify reconnect after commit-before-ack.
- [x] **G2-16** Specify duplicate retry after acknowledged commit.
- [x] **G2-17** Specify protocol behavior for an old client without batch IDs.
- [x] **G2-18** Add mixed-version contract tests.
- [x] **G2-19** Add forged-actor and future-clock security tests.
- [x] **G2-20** Add protocol corpus tests for unknown fields/message types.

**Exit evidence:** ADR-0016 and resolved ADR-0018, version fixtures, and contract
tests for ack loss, duplicate retry, actor forgery and clock skew.

## Gate 3 — Durable command and exact snapshot transaction model

- [x] **G3-01** Extend the operation log with idempotent batch append.
- [x] **G3-02** Persist replica ID, batch ID and durable sequence.
- [x] **G3-03** Add a uniqueness constraint for deduplication.
- [x] **G3-04** Return the prior durable result for a duplicate batch.
- [x] **G3-05** Introduce schema versioning and ordered migrations.
- [x] **G3-06** Test migration from every supported schema fixture.
- [x] **G3-07** Change command order to validate → append → apply → ack → broadcast.
- [x] **G3-08** Inject append failure and prove no room mutation.
- [x] **G3-09** Inject disconnect after append and prove retry deduplication.
- [x] **G3-10** Capture snapshot document with its exact covered sequence.
- [x] **G3-11** Change snapshot-store API to accept captured coverage.
- [x] **G3-12** Remove write-time `next_seq` inference.
- [x] **G3-13** Race snapshot write with later appends in a deterministic test.
- [x] **G3-14** Truncate only operations covered by the committed snapshot.
- [x] **G3-15** Make snapshot commit and eligible truncation atomic.
- [x] **G3-16** Separate snapshot capture, serialization and write timings.
- [x] **G3-17** Bound concurrent snapshot jobs and retained snapshot memory.
- [x] **G3-18** Define retry/backoff after snapshot failure without blocking edits.
- [x] **G3-19** Make restore corruption/unavailability fail closed.
- [x] **G3-20** Quarantine or diagnose corrupt snapshots without deleting evidence.
- [x] **G3-21** Define operation-log recovery when snapshot decode fails.
- [x] **G3-22** Decide WAL checkpoint policy and expose checkpoint health.
- [x] **G3-23** Test disk-full, read-only and database-busy outcomes.
- [x] **G3-24** Measure and choose a fair bounded storage writer strategy.
- [x] **G3-25** Record tombstone/offline-replica collection horizon in an ADR.
- [x] **G3-26** Add log/snapshot retention and database growth tests.

**Exit evidence:** ADR-0017 and ADR-0020 through ADR-0022, schema fixtures,
crash/retry/race/fault/retention tests, the measured storage baseline and live
storage-health/restart checks prove durable-before-visible and exact replay
with no acknowledged loss.

## Gate 4 — Room-cell primitive

- [x] **G4-01** Define the room-cell command enum.
- [x] **G4-02** Define typed response channels and terminal outcomes.
- [x] **G4-03** Define lifecycle states: restoring, ready, draining, failed, stopped.
- [x] **G4-04** Move materialized document ownership into one cell task.
- [x] **G4-05** Move accepted/snapshot counters into the cell.
- [x] **G4-06** Give each cell a bounded mailbox.
- [x] **G4-07** Set initial capacity from measured legitimate bursts.
- [x] **G4-08** Return explicit overload when the mailbox is full.
- [x] **G4-09** Define command deadlines and caller cancellation semantics.
- [x] **G4-10** Ensure dropped response receivers do not panic the cell.
- [x] **G4-11** Keep presence ephemeral and outside durable command append.
- [x] **G4-12** Define ordering between presence, joins, edits and departure.
- [x] **G4-13** Move join snapshot reads through the cell.
- [x] **G4-14** Move stats reads through the cell or immutable published state.
- [x] **G4-15** Move snapshot scheduling decisions into the cell.
- [x] **G4-16** Supervise cell panic/exit and mark the directory entry failed.
- [x] **G4-17** Prevent automatic empty-cell replacement after abnormal exit.
- [x] **G4-18** Add two-scope concurrency tests.
- [x] **G4-19** Add one-scope command ordering tests.
- [x] **G4-20** Add hot-scope overload without cold-scope degradation test.

**Exit evidence:** ADR-0023, `room_cell.rs`, the 64-command measured baseline,
and deterministic FIFO/panic/drop/overload/two-scope tests prove there is no
server-wide document mutex and that unrelated cells make progress.

## Gate 5 — Scope directory and lifecycle

- [x] **G5-01** Replace `HashMap<String, Room>` with lightweight cell handles.
- [x] **G5-02** Minimize directory critical-section duration.
- [x] **G5-03** Coalesce concurrent cold joins into one restore.
- [x] **G5-04** Bound simultaneous restores.
- [x] **G5-05** Bound active, restoring and failed entries independently.
- [x] **G5-06** Preserve “read endpoints never create rooms”.
- [x] **G5-07** Define empty, disconnected and idle precisely.
- [x] **G5-08** Implement idle TTL as a cell stop handshake.
- [x] **G5-09** Reject new work once draining starts.
- [x] **G5-10** Drain queued durable commands before stop.
- [x] **G5-11** Confirm no snapshot job or broadcast task retains a dead cell.
- [x] **G5-12** Remove stopped entries only after task termination.
- [x] **G5-13** Add rejoin-during-drain behavior and tests.
- [x] **G5-14** Add failed-entry recovery policy and operator signal.
- [x] **G5-15** Prevent restore stampedes after transient database failure.
- [x] **G5-16** Use hashed/bounded scope correlation in logs and metrics.
- [x] **G5-17** Load-test directory churn at the room-count limit.
- [x] **G5-18** Verify bounded task/memory overhead for 10,000 inactive scopes.

**Exit evidence:** `directory.rs`, ADR-0023, directory health, coalesced-open,
failed-tombstone, durable-drain, rejoin-during-drain and 10,000-entry churn tests
prove no duplicate owner, inactive task retention, partial restore or stampede.

## Gate 6 — Security boundaries and resource governance

- [x] **G6-01** Define allowed WebSocket Origin policy per deployment mode.
- [x] **G6-02** Validate Origin during browser handshake and test rejection.
- [x] **G6-03** Remove captured bearer query data from browser history promptly.
- [x] **G6-04** Evaluate fragment, secure cookie or short-lived bootstrap alternatives.
- [x] **G6-05** Confirm tokens never appear in logs, metrics or error bodies.
- [x] **G6-06** Decide long-session expiry/revalidation behavior.
- [x] **G6-07** Define authorization behavior for reconnect and resumed outbox.
- [x] **G6-08** Add per-identity aggregate rate budgets.
- [x] **G6-09** Add per-scope aggregate rate and mailbox budgets.
- [x] **G6-10** Prevent many connections from multiplying one scope's limits.
- [x] **G6-11** Bound decompression if compression is ever negotiated.
- [x] **G6-12** Bound restore, snapshot and serialization CPU/memory work.
- [x] **G6-13** Threat-model room directory, mailbox and storage scheduler.
- [x] **G6-14** Threat-model replica/batch identity replay and enumeration.
- [x] **G6-15** Test cross-scope command, dedupe and snapshot isolation.
- [x] **G6-16** Test malicious lagging subscribers and reconnect storms.
- [x] **G6-17** Review dependencies and unsafe surface after refactor.
- [x] **G6-18** Update security deployment guidance and incident signals.

**Exit evidence:** ADR-0024, the repository threat model, deployment/incident
guide, dependency/unsafe review, bounded resource tests and live auth/origin/
expiry/cross-scope/reconnect/lag tests cover browser, identity, scope, store and
resource boundaries.

## Gate 7 — Client durable outbox and recovery UX

- [x] **G7-01** Stop clearing outbox entries on WebSocket `send`.
- [x] **G7-02** Remove entries only on matching durable ack.
- [x] **G7-03** Persist outbox and replica identity in IndexedDB.
- [x] **G7-04** Restore pending batches before opening the socket.
- [x] **G7-05** Preserve original batch IDs across reconnect/reload.
- [x] **G7-06** Bound outbox bytes and operation count.
- [x] **G7-07** Surface local-only, sending, durable and refused states.
- [x] **G7-08** Back off reconnect with jitter and an upper bound.
- [x] **G7-09** Avoid flushing edits before init/version negotiation completes.
- [x] **G7-10** Reconcile server init with pending local operations deterministically.
- [x] **G7-11** Handle duplicate ack and ack for unknown/expired batch.
- [x] **G7-12** Handle overload without busy retry.
- [x] **G7-13** Handle authorization expiry without discarding local work.
- [x] **G7-14** Handle permanent semantic refusal with export/recovery UX.
- [x] **G7-15** Test close before commit, after commit and before ack.
- [x] **G7-16** Test browser reload with pending work.
- [x] **G7-17** Test two tabs with distinct stable replica identities.
- [x] **G7-18** Test storage quota failure and private-browsing limitations.
- [x] **G7-19** Keep accessibility announcements aligned with durability state.
- [x] **G7-20** Document offline guarantees and explicit limits.

**Exit evidence:** ADR-0025, deterministic outbox tests and rendered browser
automation prove no pending batch disappears across socket close, lost ack or
reload before its durable acknowledgement; refusal recovery remains exportable.

## Gate 8 — FFI isolation, performance and observability

- [x] **G8-01** Measure registry-lock wait/hold time by operation class.
- [x] **G8-02** Replace registry values with per-board synchronized handles.
- [x] **G8-03** Hold the registry lock only for handle lookup/open/close.
- [x] **G8-04** Execute board mutation under its own lock.
- [x] **G8-05** Serialize scene/snapshot without blocking unrelated handles.
- [x] **G8-06** Define close racing with an in-flight operation.
- [x] **G8-07** Preserve stale-handle and panic-containment behavior.
- [x] **G8-08** Stress independent handles across native threads.
- [x] **G8-09** Re-benchmark wasm and native FFI paths.
- [x] **G8-10** Instrument directory lookup and cold restore latency.
- [x] **G8-11** Instrument mailbox depth, wait and overload count.
- [x] **G8-12** Instrument validation, append, apply, ack and broadcast latency.
- [x] **G8-13** Instrument snapshot capture/serialize/write separately.
- [x] **G8-14** Instrument storage queue wait independently of SQL time.
- [x] **G8-15** Define bounded-cardinality metric labels and buckets.
- [x] **G8-16** Add structured correlation IDs for replica/batch/sequence.
- [x] **G8-17** Establish per-scope fairness and cross-scope p99 SLOs.
- [x] **G8-18** Compare baseline and target on identical workloads.
- [x] **G8-19** Add regression thresholds where CI hardware is stable enough.
- [x] **G8-20** Update benchmark documentation with commands and raw artifacts.

**Exit evidence:** ADR-0026, fixed-cardinality telemetry, parallel-handle tests,
native/wasm benchmarks and the eight-scope SLO workload demonstrate lower
independent-work contention without convergence regressions.

## Gate 9 — Operations, recovery and production lifecycle

- [x] **G9-01** Split liveness from readiness endpoints.
- [x] **G9-02** Include database writable/readable checks in readiness policy.
- [x] **G9-03** Define readiness during restore backlog and storage saturation.
- [x] **G9-04** Add degraded-but-live diagnostic state.
- [x] **G9-05** Implement process drain: stop admission, drain cells, flush storage.
- [x] **G9-06** Bound drain time and report incomplete scopes.
- [x] **G9-07** Test termination during queued, appended and snapshotting commands.
- [x] **G9-08** Define crash consistency at every command transition.
- [x] **G9-09** Implement online SQLite backup with integrity validation.
- [x] **G9-10** Automate restore into an isolated verification database.
- [x] **G9-11** Schedule backup/restore drills and record RPO/RTO evidence.
- [x] **G9-12** Define WAL checkpoint, retention and disk alert thresholds.
- [x] **G9-13** Define VACUUM/compaction maintenance without global outage.
- [x] **G9-14** Alert on corrupt scope, restore storm, dedupe growth and disk-full.
- [x] **G9-15** Document database migration rollback/forward-fix procedure.
- [x] **G9-16** Document incident response for partial storage availability.
- [x] **G9-17** Document capacity model for cells, mailboxes and snapshots.
- [x] **G9-18** Add startup validation of configuration and persistent paths.
- [x] **G9-19** Add release smoke test against an upgraded real database fixture.
- [x] **G9-20** Run and record a disaster-recovery game day.

**Exit evidence:** ADR-0027, readiness/drain/transition/storage tests, monthly CI
drills and the recorded hard-kill backup/restore game day meet the published
zero-acknowledged-loss and 30-second recovery objectives.

## Gate 10 — Deployment topology and scale decision

- [x] **G10-01** Keep one server process as the authoritative initial topology.
- [x] **G10-02** State that two active writers for one scope are unsupported.
- [x] **G10-03** Measure the single-process ceiling before adding distribution.
- [x] **G10-04** Define placement-key and ownership requirements for horizontal scale.
- [x] **G10-05** Evaluate sticky routing versus leased scope ownership.
- [x] **G10-06** Define split-brain prevention and fencing requirements.
- [x] **G10-07** Define cell handoff and drain during deployment.
- [x] **G10-08** Define presence fan-out requirements across processes.
- [x] **G10-09** Define database concurrency implications for multiple processes.
- [x] **G10-10** Run ATAM review before choosing a distributed topology.
- [x] **G10-11** Record any topology change in a superseding ADR.
- [x] **G10-12** Do not extract services solely to mirror source modules.

**Exit evidence:** ADR-0028's ATAM keeps the single-process topology after 64
simultaneously writing scopes qualified and 96 produced explicit bounded
overload; any scale-out now requires leased ownership and durable fencing.

## Gate 11 — Final qualification and governed delivery

- [x] **G11-01** Run format, clippy, unit, integration and doc tests.
- [x] **G11-02** Run exhaustive convergence and property suites.
- [x] **G11-03** Build native, wasm32, cdylib and staticlib targets.
- [x] **G11-04** Run protocol compatibility and mixed-client suites.
- [x] **G11-05** Run restart durability and fault-injection suites.
- [ ] **G11-06** Run browser offline/reload/reconnect suites.
- [x] **G11-07** Run authentication, origin and cross-scope negative suites.
- [ ] **G11-08** Run dependency audit, policy and unsafe-surface checks.
- [x] **G11-09** Run performance/isolation workload against baseline.
- [x] **G11-10** Validate architecture docs, ADR links and task status.
- [x] **G11-11** Review schema migration and downgrade/forward-fix notes.
- [x] **G11-12** Review operational dashboards and alert playbooks.
- [x] **G11-13** Record unresolved risks with owners and revisit triggers.
- [x] **G11-14** Prepare rollback and data-compatibility plan.
- [x] **G11-15** Use a focused PR with architecture, risk and evidence summary.
- [ ] **G11-16** Require architecture/security ownership review.
- [ ] **G11-17** Merge only with protected checks passing.
- [ ] **G11-18** Validate the immutable artifact in the target environment.
- [ ] **G11-19** Run post-deploy smoke and data-recovery checks.
- [ ] **G11-20** Close the program only after production evidence is recorded.

## Execution ledger

Update this table as slices complete. Do not mark a task complete solely because
code exists; name its evidence.

| Slice | Tasks | Status | Evidence |
|---|---|---|---|
| 0A — audit and authority | G0-01..G0-12 | Complete | Architecture and program documents; baseline commands recorded in task handoff |
| 0B — controlling ADRs/check | G0-13..G0-17 | Complete | ADR-0015..0017 accepted; ADR-0018 proposed with acceptance evidence; architecture check passes |
| 0C — baseline inventory | G0-18 | Complete | `docs/benchmarks.md` is reproducible and explicitly identifies missing concurrency, memory and database-size evidence routed to Gate 8 |
| 1A — prepared validation | G1-01..G1-18 | Complete | Core, room and protocol tests cover whole-batch semantics, key/value compatibility, generated capacity shapes, malformed input, empty batches and accepted stale no-ops |
| 2A — versioned durable delivery | G2-01..G2-20 | Complete | ADR-0016, ADR-0018 and protocol-v2 fixtures/unit/live tests prove negotiated compatibility, scope-bound stable actors, idempotent durable ack and typed refusal |
| 3A — exact durable storage | G3-01..G3-26 | Complete | ADR-0017 and ADR-0020..0022, migration fixtures, storage fault/race/retention tests and live restart/storage-health checks |
| 4A — bounded room cells | G4-01..G4-20 | Complete | ADR-0023, `room_cell.rs`, measured 64-command burst, FIFO/two-scope/full-mailbox/drop/panic tests |
| 5A — directory lifecycle | G5-01..G5-18 | Complete | `directory.rs`, lifecycle health, coalesced restore, failed tombstone, durable drain/rejoin and 10,000-entry churn tests |
| 6A — security/resource governance | G6-01..G6-18 | Complete | ADR-0024, threat model, deployment/incident and dependency reviews, live auth/Origin/expiry tests, isolation/lag/reconnect/resource tests |
| 7A — durable client recovery | G7-01..G7-20 | Complete | ADR-0025, IndexedDB outbox tests and rendered offline/reload/reconnect proof cover exact IDs, lost acks, refusal export and accessibility state |
| 8A — isolation and observability | G8-01..G8-20 | Complete | ADR-0026, per-board lock/race tests, fixed-cardinality phase telemetry, native/wasm benchmarks and eight-scope SLO evidence |
| 9A — production lifecycle | G9-01..G9-20 | Complete | ADR-0027, readiness/drain/storage tests, scheduled backup/restore drill and recorded RPO 0 / 154.53 ms RTO game day |
| 10A — topology and scale | G10-01..G10-12 | Complete | ADR-0028 ATAM; 64 writing scopes qualified, 96 bounded-overload trigger; lease/fencing requirements recorded before scale-out |
| 11A — local qualification and governed draft | G11-01..05, G11-07, G11-09..15 | Partial (13/20) | Full workspace/build targets, live protocol/restart/auth/recovery and 64-scope workload pass; ADR-0029, dashboard contract, release record and draft PR #14; browser reload, security, independent review/protected merge and target evidence remain open |

## Definition of done

“Per-scope room-cell modular monolith” is complete only when the code topology,
runtime ownership, protocol delivery semantics, persistence recovery, client
behavior, security controls, operational evidence and architecture documents all
agree. Renaming `Room` to `RoomCell` or replacing one mutex with tasks does not
meet this definition.
