# Decision records

One file per decision that shapes Axel: what we chose, why, what it costs.
`DESIGN.md` says what the system *is*; an ADR says why it is that way, so the
reasoning survives the people and the chat threads that produced it.

Rough ideas, open questions and risky research don't go here; they go to
`../rfd/` and come back as an ADR once settled.

## Format

Markdown, one decision per file, numbered in the order written:
`NNNN-short-slug.md`. Numbers are never reused; a superseded record stays and
says what replaced it. Each file:

```
# ADR NNNN: <the decision, as a sentence>

Status: proposed | accepted (YYYY-MM-DD) | superseded by NNNN | rejected

## Context      what was true, what we knew, what forced a choice
## Decision     what we do, in the present tense
## Consequences what it costs, what it rules out, what it makes easy
## Possible future improvements   optional
## References   optional
```

Keep it under a page. Link to the `DESIGN.md` section it explains and to the
`RESEARCH.md` notes it draws on; don't repeat them. A record written after the
fact says so (`accepted (retroactive, …)`).

## When to write one

- A choice between real alternatives that a newcomer would question
  ("why Ed25519 and not secp256k1?", "why no daemon?").
- A reversal: we did X, now we do Y.
- A study of another system that changed or confirmed a choice.

Not for: how a feature works (that's `DESIGN.md`), an API shape (`CONTRACTS.md`),
or a plan with open questions (an RFD).

## Index

| # | Decision | Status |
|---|---|---|
| [0001](0001-turn-checkpoints-not-commit-checkpoints.md) | Checkpoints per turn, in the working tree, not per commit | accepted |
| [0002](0002-terminal-first-on-rex.md) | Terminal first: Rex is the window manager, Axel draws no windows | accepted |
| [0003](0003-structured-signals-never-scraping.md) | Agent status comes from protocols, never from reading their screens | accepted |
| [0004](0004-agents-are-ed25519-principals.md) | Agents are named Ed25519 principals attested by their owner | accepted |
| [0005](0005-signed-event-log-local-first.md) | The signed event log is the truth; local-first, replicas merge by union, no daemon | accepted |
| [0006](0006-threads-are-the-primitive.md) | The thread is the primitive; sessions move, threads don't; ids are Axel's | accepted |
| [0007](0007-capabilities-not-harness-names.md) | Harness behaviour is driven by capabilities, not by matching on names | accepted |
| [0008](0008-autopilot-with-a-guard.md) | Autopilot by default, with a guard hook for the irreversible | accepted |
| [0009](0009-rebuild-by-importing-not-trimming.md) | Rebuild from scratch, importing modules one by one with provenance | accepted |
