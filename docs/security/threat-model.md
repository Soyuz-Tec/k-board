# k-board repository threat model

## Overview

k-board is a multi-tenant collaborative canvas delivered in two forms: a
standalone Rust HTTP/WebSocket server with a browser client, and an embeddable
Rust engine exposed through a raw C ABI. The primary runtime code is under
`crates/` and `web/`; scripts and CI exercise the deployed protocol but are not
production request handlers.

The security-critical assets are:

- the confidentiality and integrity of every scope's board, operation log,
  snapshots, batch identities and presence stream;
- the durability meaning of an acknowledgement;
- bearer grants, server signing secrets and stable replica identities;
- availability of the server, room directory, cell mailboxes and SQLite writer;
- the memory safety and process integrity of native hosts using `kboard-ffi`;
- recovery evidence in a corrupt or partially unavailable database.

The highest-value invariants are that authority for one scope never grants
access to another, one scope never owns another scope's mutable state, refusal
never partially mutates or persists a batch, an acknowledgement is emitted only
after durable commit, and untrusted input cannot create unbounded work or memory.

## Threat Model, Trust Boundaries, and Assumptions

### Actors and capabilities

- An unauthenticated Internet client can choose paths, headers and handshake
  timing. Public deployments must prevent it from reaching a writable session.
- An authenticated but malicious collaborator controls its scope's WebSocket
  frames, replica and batch identifiers, operation content, ordering, reconnect
  timing and connection count. It must not cross scope boundaries or exhaust the
  whole process.
- A hostile website can cause a user's browser to attempt WebSocket handshakes
  with an ambiently available grant. Exact Origin policy is defense in depth.
- An operator controls bind address, allowed origins, signing secret, database
  path, TLS termination, filesystem permissions, backup and process topology.
- An embedding host controls FFI pointers, lengths, handles, clocks, authority
  and persistence adapters. Invalid native inputs are plausible integration
  errors even when the host is not malicious.
- A repository contributor or compromised dependency can affect build output.
  Branch review, CI, advisory, licence and source policy are the controls.

### Trust boundaries

1. **Network to transport.** `crates/kboard-server/src/main.rs` accepts route,
   headers and WebSocket messages. Scope syntax, frame size, protocol version,
   Origin, authentication and rate checks occur here.
2. **Grant to scope authority.** `auth.rs` verifies signature, expiry and exact
   scope binding. Reconnect and active-session expiry are authorization events,
   not merely transport events.
3. **Transport to room cell.** `directory.rs` maps a validated scope to one
   lifecycle-bearing handle. `room_cell.rs` admits typed work through a bounded
   FIFO mailbox and owns all mutable state for that scope.
4. **Room cell to durable store.** `storage_writer.rs` serializes bounded work to
   `kboard-store`. Durable append must precede apply, ack and broadcast. Restore
   errors fail closed and retain failed tombstones instead of creating empties.
5. **Browser URL to in-memory grant.** `web/app.js` captures a bootstrap query
   grant and immediately removes it from browser history. The grant remains
   sensitive while held in memory and while offered in the handshake.
6. **Safe Rust to native host.** `kboard-ffi` is the only permitted unsafe Rust
   boundary. It validates pointers and handles and contains panics. Core, store
   and server crates forbid unsafe code.
7. **Repository to build/release.** `Cargo.lock`, `deny.toml`, GitHub Actions and
   review policy constrain dependency sources, advisories and unreviewed change.

### Attacker-controlled inputs

Attacker-controlled data includes scope path segments, `Origin`, offered
subprotocols and extensions, bearer grants, v1/v2 JSON, replica and batch IDs,
HLC stamps, element IDs, property keys and values, cursor coordinates,
connection/reconnect cadence and socket backpressure. A malicious or damaged
local environment can also supply SQLite rows, snapshots and operation logs.
FFI pointers, lengths, JSON and handles are untrusted at the native boundary.

Environment variables, TLS configuration, signing secrets and database paths
are operator-controlled. Rust source, migrations, workflows and dependency
updates are developer-controlled and require review rather than runtime input
validation.

### Assumptions and explicit exclusions

- Public traffic is terminated by TLS before reaching the server; bearer grants
  sent over plaintext are outside the server's ability to protect.
- `KBOARD_SECRET` is high entropy, is supplied through secret management and is
  not exposed to the browser or repository.
- The database and backup are protected by host filesystem permissions.
- One SQLite database has one k-board server writer. Horizontal placement,
  leases and fencing are not implemented.
- Origin checks do not replace authentication and are intentionally absent for
  non-browser clients that send no Origin.
- Compromise of the operating system, TLS proxy, signing secret or embedding
  host is outside application isolation, but secure failure and useful incident
  evidence remain objectives.
- Rendering defects without script execution, data exfiltration or integrity
  impact are product bugs rather than security vulnerabilities.

## Attack Surface, Mitigations, and Attacker Stories

### Cross-scope access and enumeration

A client may substitute a scope in the URL, replay a grant for another scope or
probe whether a board exists. Exact scope-bound HMAC verification occurs before
directory lookup, and public failures are generic. `ScopeId` also refuses
cross-scope merge. Read endpoints use `directory.existing` and never create a
room. Logs use fixed-length scope digests rather than raw tenant identifiers.

The critical attacker story is an authenticated grant for scope A reading,
mutating or learning the existence of scope B. Tests cover authorization,
cross-scope command/dedupe sequencing, storage and snapshot isolation.

### Replica and batch replay

An authenticated peer controls stable replica and batch IDs and can retry after
losing an acknowledgement. v2 derives its actor from scope plus replica,
requires actor-bound new element IDs, limits future-clock skew, and stores the
scope/replica/batch outcome atomically with operations. The same identity and
payload returns the earlier sequence; a conflicting payload is refused. A
replica cannot be concurrently claimed twice in one scope, and one identity is
bounded across scopes.

Enumeration of another replica or batch must not reveal its payload or another
scope. IDs are opaque and public errors are typed but content-free. The retained
dedupe horizon is an operational contract, so very old retries can become new
commands only after the documented expiry.

### Browser credential and handshake abuse

A hostile site may try to use a leaked or ambient bearer. Browser Origin values
are exact-match allowlisted; public binds require an explicit allowlist. The
query bootstrap grant is removed from history before application startup, and
the response never selects the bearer as a WebSocket protocol. Offered
compression is ignored and never negotiated, avoiding unbounded decompression.
Active sessions close at grant expiry and reconnect requires fresh verification.

XSS remains important because same-origin script can read in-memory authority
and board content. CSP, no-referrer, no-sniff and frame-denial headers reduce the
surface; the no-bundler client and same-origin static files reduce supply-chain
script exposure. HTML/DOM changes must continue to treat board strings as data.

### Resource exhaustion and fault isolation

Attackers can open many sockets, flood frames, create scopes, fill mailboxes,
force restores, submit large documents or lag subscriptions. Controls include
process/scope/identity connection caps, layered token buckets, bounded frame and
batch sizes, element and payload ceilings, bounded room and storage mailboxes,
bounded concurrent restores, directory lifecycle caps, broadcast lag closure,
snapshot/restore byte and operation limits, deadlines and idle drain.

A hot room may saturate its own mailbox and receive overload, but it must not
hold a global document lock or stop a cold room. SQLite remains a shared single
writer, so its queue is separately bounded and all callers receive typed
unavailable/overload outcomes. Failed restores retain bounded tombstones and
backoff to prevent retry stampedes.

### Persistence corruption and durability confusion

Malformed snapshots, incomplete logs, read-only/full/busy databases and process
failure can target recovery. Store migrations are versioned, snapshot coverage
is captured exactly, commit and eligible truncation are atomic, and corrupt
snapshot evidence is retained. Recovery from a corrupt snapshot is allowed only
when the complete log is provably available; otherwise join fails closed.

The most damaging integrity story is an acknowledgement or peer broadcast for
state that restart cannot reconstruct. Room-cell ordering is prepare, durable
append, apply, acknowledgement and broadcast. Append failure cannot mutate or
advance in-memory state.

### Native embedding and supply chain

Invalid FFI pointers, lengths and handles could corrupt a host process if unsafe
code escapes its narrow adapter. `kboard-core`, `kboard-store` and
`kboard-server` forbid unsafe code; only `kboard-ffi` performs pointer access,
with validation, handle synchronization and panic containment. Any new unsafe
site outside that crate is a security-policy violation.

Dependency compromise can affect all deployments and embedding hosts. Locked
dependencies, restricted registries/sources, advisory CI and licence/bans checks
are required. Duplicate transitive versions are reviewed but not automatically
blocked when they are separate proc-macro generations.

## Severity Calibration (Critical, High, Medium, Low)

### Critical

- unauthenticated remote code execution or memory corruption in the standalone
  server;
- a remotely exploitable FFI flaw that predictably executes code in common host
  integrations;
- compromise of the signing secret or a universal authentication bypass that
  grants every scope.

### High

- cross-scope board read/write, scope-authority bypass or broad private-board
  enumeration;
- durable acknowledgement of data that is not recoverable, or attacker-driven
  log/snapshot corruption affecting unrelated scopes;
- unauthenticated process-wide resource exhaustion under ordinary public
  deployment assumptions;
- stored XSS that steals live grants or acts across boards.

### Medium

- denial of service confined to the attacker's authorized scope but requiring
  operator intervention;
- replica/batch replay that duplicates or suppresses edits within one scope;
- token disclosure through application diagnostics, response headers or browser
  history with realistic access by another principal;
- bypass of expiry that extends an otherwise valid scoped session.

### Low

- bounded transient availability loss with automatic recovery;
- low-rate scope correlation that does not reveal raw identifiers or content;
- development-only insecure configuration that is restricted to loopback and
  clearly announced;
- dependency duplication or hardening gaps without a reachable vulnerable path.

Severity can move upward with cross-scope reach, unauthenticated access,
persistence across restarts, broad default exposure or reliable host-process
impact. It moves downward when exploitation requires a malicious operator,
physical database access already sufficient to read all boards, or a deliberately
unsupported multi-process topology.
