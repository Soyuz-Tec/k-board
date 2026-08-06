# ADR-0029: Qualify one immutable release candidate

- **Status:** Accepted

## Context

The server, native FFI libraries, wasm engine and browser host form one
deployable compatibility set. Rebuilding any member between qualification and
deployment can introduce behavior that the recorded tests did not exercise.
The repository also has no configured production environment, so a successful
build must not be presented as a production deployment.

## Decision

CI assembles one commit-addressed release candidate containing the release
server, native dynamic and static libraries, wasm engine, browser assets,
license, README and a SHA-256 manifest. CI validates the manifest and starts the
packaged server against the packaged web and wasm assets before uploading the
unchanged directory as a retained workflow artifact.

Promotion must use that exact artifact and verify its manifest in every target
environment. It must not rebuild from source. Production promotion remains
blocked until a named target environment, persistent volume, secret boundary,
backup location and accountable operator exist.

Database migrations are forward-only. Rollback may replace the executable and
assets only when the selected older binary supports the on-disk schema.
Otherwise operators preserve the database and WAL and deploy a forward fix.
Restoring an older backup requires an explicit accepted recovery-point loss.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Rebuild separately in each environment | Produces different, unqualified bits |
| Publish only the server binary | Omits the FFI, wasm and web compatibility set |
| Treat CI success as deployment evidence | Confuses build proof with target-environment proof |
| Automatically downgrade the database on rollback | Risks irreversible loss under a newer schema |

## Consequences

- Every candidate has one commit identity and a complete checksum manifest.
- Release evidence can name the exact bits that were tested and promoted.
- Artifact retention is finite; a long-lived release needs separate governed
  retention before the CI artifact expires.
- Target validation, post-deploy smoke and recovery evidence remain separate
  gates and cannot be inferred from packaging.

## Validation

- CI builds all release targets and generates `SHA256SUMS` over the bundle.
- CI verifies every checksum before starting the packaged server.
- The packaged server must pass readiness and wasm-asset smoke checks.
- A target promotion must record the artifact name, commit, manifest digest,
  environment, operator, probe results and restore evidence.

## Revisit triggers

- Signing or provenance requirements exceed a checksum manifest.
- Artifact retention is shorter than the supported rollback window.
- The browser host or native library acquires an independent release cadence.
- A deployment orchestrator or multi-process topology is introduced.
