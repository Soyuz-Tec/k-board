# k-board working agreements

These instructions apply to the whole repository.

## Architecture authority

- `docs/ARCHITECTURE.md` is the architecture source of truth.
- `docs/architecture/room-cell-modular-monolith-program.md` is the ordered
  transformation plan. Complete its gates in order; do not implement a later
  phase by weakening an earlier invariant.
- Material changes to protocol, persistence, identity, concurrency, security,
  deployment, or public FFI require an ADR in `docs/adr/` before implementation.
- Preserve the dependency direction: hosts/adapters depend on `kboard-core`;
  `kboard-core` never depends on a server, store, transport, runtime, clock, or
  entropy source.

## Non-negotiable invariants

- Scope is the tenancy and room-cell boundary. Never share mutable board state
  across scopes.
- Validate an operation batch as a whole. Refusal must not partially mutate,
  persist, acknowledge, or broadcast it.
- A successful acknowledgement means the batch is durably recoverable.
- Broadcast only acknowledged operations. Never expose state that recovery
  cannot reconstruct.
- Snapshots carry the exact log sequence they include. Never infer coverage at
  snapshot-write time.
- The only permitted Rust `unsafe` code is the audited C ABI in `kboard-ffi`.
- Do not hard-code secrets or log bearer tokens.

## Change and verification discipline

- Keep each change focused and update tests for behavior changes.
- Apply the reviewability thresholds in
  `docs/architecture/reviewability-standard.md`. They trigger review and an
  explanation, not automatic rejection: roughly 75 logical lines per function,
  600 physical lines per source file, 500 changed source lines per change, or
  20 changed files. Generated code, fixtures, migrations, vendored code and
  mechanical changes are handled by the documented exceptions.
- Prefer extracting a cohesive responsibility when a threshold is crossed. Do
  not split code merely to satisfy a number, and do not waive an architecture,
  security, durability or test invariant because a change is small.
- Run `node scripts/architecture-check.mjs`, formatting, clippy, the relevant
  tests, and the end-to-end checks affected by a change.
- Run `node scripts/review-size-report.mjs` for an advisory source-file report;
  on a PR, pass `--base <target-ref>` or let CI derive the target branch.
- Update the architecture program status and ADR index in the same change that
  completes an architectural gate.
- Report exactly which checks ran and which did not.
