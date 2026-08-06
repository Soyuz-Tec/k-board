# Dependency and unsafe-code review — 2026-08-05

Scope: the full workspace after the room-cell, directory and resource-governance
refactor.

## Results

- `cargo deny check` passed advisories, bans, licences and sources. It reported
  two non-blocking warnings: the allowed ISC licence is currently unused, and
  `syn` 2.x/3.x are both present through separate transitive proc-macro trees.
- `cargo tree --workspace -d` confirmed the only duplicate family is `syn`.
  Version 2 is used by futures/zerocopy dependencies; version 3 is used by
  serde, Tokio and thiserror proc macros. No runtime parser or network surface
  is duplicated by this warning.
- `cargo audit` was not installed on the review workstation. The repository's
  GitHub Actions audit job installs `cargo-audit --locked` and runs it on every
  CI execution; `cargo-deny`'s advisory phase passed locally.
- Source search found unsafe Rust only in `crates/kboard-ffi`, the documented raw
  C ABI boundary and its tests. `kboard-core`, `kboard-store` and
  `kboard-server` forbid unsafe code at crate level. The refactor introduced no
  unsafe block or function.

## Decision

No dependency or unsafe-code change is required for Gates 4–6. Keep the `syn`
duplicate warning visible until upstream dependencies converge; pinning proc
macro versions merely to silence it would increase maintenance and supply-chain
risk. A release still requires the remote `cargo-audit` job to pass.
