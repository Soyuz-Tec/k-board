# Gate 11 release qualification

This record separates local qualification from GitHub governance and target
deployment. A checked item needs the named evidence; one phase cannot stand in
for another.

## Local qualification

- Format, clippy, workspace unit/integration tests and doc tests pass.
- Native server, native cdylib/staticlib and wasm32 release targets build.
- Protocol-v2, restart durability, storage-health, backup/restore, room-cell
  SLO, outbox and architecture checks pass.
- A fresh rendered session reached `offline · edits retained`. The Gate 7
  rendered reload/reconnect evidence remains valid, but the Gate 11 offline
  reload rerun was blocked by the browser's localhost URL policy and is open.
- The dependency/security qualification item is intentionally open because the
  repository owner directed this execution to avoid the security check.

## Governed delivery

- Draft pull request: <https://github.com/Soyuz-Tec/k-board/pull/14>.
- CI produces `k-board-<commit>` and its `SHA256SUMS`, verifies the manifest,
  starts the packaged server and uploads the exact bundle.
- The pull request records architecture impact, review-size exceptions,
  verification, schema compatibility, rollout and rollback.
- `CODEOWNERS` identifies owned architecture and runtime surfaces.
- Independent ownership review is open until a second accountable collaborator
  can review the pull request.
- Protected merge is open until required checks pass under a main-branch rule.

## Target evidence

No staging or production environment is configured in this repository. The
following remain open and must not be inferred from local or CI success:

1. Verify the exact artifact and checksum manifest in a named target.
2. Run readiness, wasm, authenticated durable mutation and restart-recovery
   smoke checks there.
3. Verify a target backup through an isolated restore.
4. Record target, artifact, commit, manifest digest, operator and timestamps.

## Unresolved risks

| Risk | Owner | Required resolution / revisit trigger |
|---|---|---|
| Security qualification intentionally not run | Repository owner | Run the governed security criterion before final program closure |
| Sole collaborator cannot supply independent review | Repository owner | Add an accountable reviewer before protected merge |
| No target environment or deployment operator | Repository owner | Configure target, storage, secrets, backup and operator before promotion |
| CI artifact expires after 30 days | Release owner | Copy a released bundle to governed retention before expiry |
| SQLite topology remains single-writer | Architecture owner | Implement leased ownership and fencing before horizontal writers |
| Target dashboard and alert routing are not provisioned | Operations owner | Provision the dashboard contract in the named target environment |
