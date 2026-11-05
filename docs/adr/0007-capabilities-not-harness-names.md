# ADR 0007: Harness behaviour is driven by capabilities, not by matching on names

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Principles" 6.

## Context

Claude Code has hooks and a permission mode flag; Codex has `notify` and a
sandbox flag; ACP agents have in-protocol permissions; a custom command may
have none of these. v1 special-cased each harness by name in several places,
and every new harness meant finding them all. t3code solved the same problem
with capability flags per provider and per server (`RESEARCH.md` "t3code").

## Decision

A harness is described by a capability struct (hooks? ACP? status escapes?
resume flag? runtime-mode flags?) and the code that launches, guards or reads
a session asks the capabilities, never `match harness`. The harness name is
for display and for the driver that installs skills in its expected location.
A custom command with no capabilities still works: it gets the `axel status`
CLI and nothing else.

## Consequences

- Adding a harness is one capability description plus, if it has one, a
  hook adapter; the TUI, store and launcher don't change.
- Features degrade by capability: a harness without hooks shows no live
  tool calls; one without a resume flag can't be moved.
- The managed (ACP) runtime is the great equaliser: once an agent speaks ACP,
  it has every capability the thread view needs.
