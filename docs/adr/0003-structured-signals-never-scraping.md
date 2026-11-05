# ADR 0003: Agent status comes from protocols, never from reading their screens

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Principles" 2
and "Signals".

## Context

Axel v1 inferred what an agent was doing by `capture-pane` and grep, scraped
OTEL output for tokens, rang on Codex's bell, and answered prompts with
`send-keys`. Task-to-terminal identity was inferred from shared hook files. It
was the root cause of the todo/inbox pain: wrong pane, wrong task, stale state
(`RESEARCH.md` "Axel v1"). Every harness we care about now has a structured
channel: Claude Code hooks, Codex `notify`, ACP frames, and Rex's OSC 7501
program status.

## Decision

Every signal comes through a protocol. Claude hooks and Codex notify call
`axel hook`; ACP sessions are driven by `axel-acp`; anything else reports with
`axel status`. Hooks address a *session* through environment variables Axel
sets (`AXEL_THREAD`, `AXEL_SESSION`, `AXEL_AGENT`), never through a shared
file. Replies travel back over the protocol that asked (hook response, ACP
response) or into the agent's inbox for its next turn. Axel never types into an
agent's terminal.

## Consequences

- A harness without a structured channel gets only what it reports itself.
- Each hook invocation also writes OSC 7501, so Rex's indicators and Axel's
  inbox always agree.
- Token usage is read from the harnesses' own transcripts, not from a
  telemetry server.
- The guard (ADR 0008) is possible only because the hook sees tool calls
  before they run.
