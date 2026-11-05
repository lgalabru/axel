# ADR 0009: Rebuild from scratch, importing modules one by one with provenance

Status: accepted (retroactive, 2026-10-31). Explains the opening of `DESIGN.md`
and `IMPORTS.md`.

## Context

Axel v1 (tmux launcher + Swift app + axum hook server + OTEL scraping) had
good parts (`AXEL.md` config, skill install drivers, the ACP relay, worktree
isolation) tangled with the parts that failed (ADR 0003). Buzz had pure,
liftable modules (attestation, mentions, threading, verification) next to a
relay we don't want. Trimming either repo in place would have kept the
coupling and the history of the wrong design.

## Decision

Start a new workspace (`axel-next`, later `main`) and bring code over piece by
piece. Every imported module is listed in `IMPORTS.md` with its source and
what changed (`Schnorr→Ed25519`, "no OTEL/tmux hooks", "no isolated-worktree
squash flow"). Nothing is copied with its relay, tmux or Tauri coupling.
`DESIGN.md` is written first and is the source of truth; `CONTRACTS.md` fixes
each crate's public API before code changes shape.

## Consequences

- The old repos stay readable as references (`RESEARCH.md`), not as
  dependencies.
- A reader can tell borrowed code from new code and knows what to re-check
  when the source project changes.
- The cost is a slower start; the benefit is that v1's root causes could not
  come along by accident.
