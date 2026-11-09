# Requests for discussion

Where ideas live before they are decisions: a hunch, a sketch, a risky
research direction, a design with open questions. An RFD is allowed to be
wrong, half-finished and argued with. When one settles, the decision moves to
`../adr/` and the RFD records where it went.

The pattern is Oxide's RFD process (itself a descendant of Sun's and Joyent's),
with fewer states. The split between "proposal, discussed" and "decision,
recorded" is also how Rust (RFCs, then stabilisation) and most ADR users work.

## Format

`NNNN-short-slug.md`, numbered in the order started, numbers never reused.

```
# RFD NNNN: <title>

State: idea | discussion | parked | adopted (ADR NNNN) | abandoned
Authors: <who>  ·  Started: YYYY-MM-DD

## Problem        what hurts, who it hurts, how we know
## Sketch         the idea, as concretely as we can; code, mockups, numbers
## Open questions what we don't know yet, and how we'd find out
## Risks          what could make this a bad idea
## What we learned   appended as the discussion goes; keep the history
```

States:

- **idea**: written down so it isn't lost; nobody has committed to it.
- **discussion**: being worked on, possibly with a spike in a branch.
- **parked**: not now; the file says why and what would reopen it.
- **adopted**: a decision came out of it; link the ADR and stop editing.
- **abandoned**: tried or considered and dropped; the file says why.

An RFD can be rough. It cannot be vague about what question it is asking.

## Index

| # | Title | State |
|---|---|---|
| [0000](0000-template.md) | Template | – |
| [0001](0001-thread-controls.md) | Thread controls: a pane to reset, move, settle, archive and read a thread's sessions | discussion |
