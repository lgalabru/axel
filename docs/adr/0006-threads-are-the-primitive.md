# ADR 0006: The thread is the primitive; sessions move, threads don't; ids are Axel's

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Principles" 5
and "The thread model".

## Context

t3code separates the app's thread from the provider's session: AppThread →
Run → RunAttempt → ExecutionNode, with provider ids held as references of
varying strength, and "execution never moves between machines" (`RESEARCH.md`
"t3code"). v1 had no such split: a task *was* a terminal, so a dead pane was a
lost task.

## Decision

A thread is an intent plus the signed history of everything done in its name,
by any participant's agents, on any environment. It is attached to 0..n
projects (0 = scratch) and realized by many sessions over time: agent ×
environment × project × harness process. Harness ids (`claude --resume`,
Codex rollouts) are references a session holds, never identity. A thread
never moves; to move work, end the session here and start one for the same
thread there, resuming the harness conversation. Thread state is coarse
(`open|done|archived|deleted`) and set by people; live state is derived from
sessions. Hooks, status and usage are keyed by session.

## Consequences

- Several agents, environments and runtimes can serve one thread, and the
  thread view can show them together.
- "Move Atlas to studio" is a session end plus a session start with the
  transcript and branch carried along, which is why `axel session move` exists
  and is Claude-only for now.
- Usage and checkpoints are per session and per turn; thread totals are sums.
- Unlike t3code we do move work between machines, but only by copying what a
  session needs; the thread's log is the same everywhere (ADR 0005).
