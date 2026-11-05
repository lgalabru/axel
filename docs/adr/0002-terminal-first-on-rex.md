# ADR 0002: Terminal first: Rex is the window manager, Axel draws no windows

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Principles" 1
and "Non-goals".

## Context

Axel v1 was a tmux launcher plus a macOS app. The app duplicated what a
terminal multiplexer already does (panes, focus, tabs) and the agents' own CLIs
lost their native UX when wrapped. Rex (Superlogical) gives us floating layers,
window and block management, OSC 7501 status indicators in tab headers, an event
stream and a Lua config, all reachable from a CLI with `--json`; it has no
web or markdown block, so anything we show is a terminal program. See
`RESEARCH.md` "Rex".

## Decision

The UI is a TUI in a Rex block. Agents run as their real CLIs in Rex blocks.
Axel asks Rex for panes (`session.new_layer`, `split`, `detach_block`,
`move_block`) and never draws a window of its own. Outside Rex, the TUI runs
inline and degrades (no floating panes, no docking). A GUI is a non-goal.

## Consequences

- Everything Axel shows must work as text in a pane: the thread view streams
  as plain terminal output with native scrollback; details open in floating
  terminal panes; links are OSC 8 hyperlinks handled by a small app.
- We depend on Rex's control surface staying CLI-reachable; `axel-rex` is the
  one crate that knows it.
- Users keep every harness's native behaviour (permissions prompts, slash
  commands) in pane sessions; the managed runtime is a choice, not a wrapper
  forced on them.
