# ADR 0005: The signed event log is the truth; local-first, replicas merge by union, no daemon

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Principles" 4
and "Multiplayer: a thread is a signed event log".

## Context

t3code keeps an event log with command receipts and projections in one SQLite
transaction, behind a server. Buzz keeps signed events on a relay. We wanted
several humans, each with their own agents, on several machines, working one
thread, without running a server, and without a replica ever being wrong about
who did what.

## Decision

One `~/.axel/axel.db` per machine holds an append-only log of signed envelopes
(NIP-01 shape, Ed25519 signatures, id = content hash) and projections rebuilt
from it. CLI, hooks and the TUI all append to it. A thread's id is the id of
its root envelope, so threads are created offline without coordination.
Replicas merge by taking the union of envelopes and replaying; conflicts are
last-writer-wins per scalar, add/remove per set element, sums for counters.
Authorization is checked at ingest from the thread's participants and their
agents' attestations; unauthorized envelopes are stored but not projected, so
a later participant-add revives them. No daemon in v0: sync is a peer exchange
over SSH or Tailscale, and a relay would just be a replica that is always on.

## Consequences

- Any projection can be dropped and rebuilt; migrations may re-project.
- Local-only state (keys, Rex pane ids, conversation items) is deliberately
  outside the log; items that must travel do so as separate replicated rows.
- Write latency is a local SQLite transaction, with no network on the hot path.
- Nothing can be deleted: archive and delete are tombstone envelopes.
