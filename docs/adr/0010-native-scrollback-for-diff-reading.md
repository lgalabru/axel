# ADR 0010: Use native terminal scrollback for reading diffs

Status: accepted (2026-11-09).

## Context

The conversation and tool-call panes print ordinary terminal output. Rex owns
their scrolling, text selection, and search. The diff pane instead captured
the mouse and repainted an alternate screen. Its scrolling felt worse even
after improvements to code layout and colours. The owner wants the message
view's behaviour while staying in the terminal.

## Decision

Print the complete diff on the primary screen by default. Reuse the existing
code, syntax, and annotation renderer. Redraw only for a settled width change,
a different layout or turn, or edited notes. Keep review actions available.

Use `r` to enter the existing alternate-screen line review when the owner
wants source selection or comments. Returning without edits preserves the
printed diff. This follows [ADR 0002](0002-terminal-first-on-rex.md) and the
conversation stream implementation in `crates/axel-tui/src/stream.rs`.

## Consequences

Reading gets Rex's native scrolling and text selection. Read mode has no fixed
sidebar or pinned headers. Mouse-based source selection belongs to explicit
line review, where terminal screen coordinates remain meaningful. Both modes
use the same checkpoint data, review messages, and undo protocol. The launch
flow stays in [DESIGN.md](../DESIGN.md).

## Possible future improvements

Source-line hyperlinks could open a small comment input directly from native
scrollback, removing the need to enter line review for a single comment.
