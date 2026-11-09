# RFD 0001: Thread controls

State: discussion
Authors: Ludo, opus  ·  Started: 2026-11-06

## Problem

A thread has a launcher to start it and a composer to talk to it, but nothing in
between: resetting a thread whose context has grown slow, moving it to another
machine, settling or archiving it, or just seeing what its sessions have cost, all
meant the command line or the launcher's thread list. The tokens pane shows spend
per agent, not per thread, and nothing shows a thread's sessions side by side.

## Sketch

A floating pane over the thread, opened with Ctrl+D from the composer (and `axel
thread manage`), in the launcher's style: the thread's numbers on top, the things
you do to it below.

- **Header**: title, state, age, sessions; a table of sessions (agent, harness and
  model, state and host, turns, tool calls, tokens, cost, context fill, duration;
  columns drop from the right as the pane narrows); the thread's totals and a
  braille chart of its last hour.
- **Actions**: Reset (end the sessions, start the agent again with a fresh context
  and a recap: the intent, the last eight turns in a few lines each, the files they
  changed; then hang up the thread's composers and dock the thread again, so its
  panes run the installed binary too), Move (a live Claude session travels with its conversation, anything else
  starts on the host with the recap), Settle or Reopen, Archive, Stop, Rename. Each
  confirms with y/n; reset and move ask which agent and host when there's a choice.

Shipped as a first version in `axel_tui::manage` and `axel thread manage`.

## Open questions

- **Reset by compaction.** The recap is built from the turn summaries. Claude Code
  can compact its own conversation (`/compact`), which keeps far more; the ACP
  adapters would need to expose it. Is the recap enough in practice?
- **Per-session analytics over time.** The header is a snapshot. A per-session
  chart, or the spend by turn, would show where a thread's cost went.
- **Move for Codex sessions** waits on `axel session move` supporting them.
- **Opening from the launcher's thread list and the dashboard**, not only the
  composer.

## Risks

Reset loses the conversation on purpose; the recap must say enough or the agent
redoes work. Moving ends the local session before the remote one has proven it
started.

## What we learned

- 2026-11-06: first version shipped; the recap format is the part to watch.
