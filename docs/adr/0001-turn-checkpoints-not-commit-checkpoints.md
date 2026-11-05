# ADR 0001: Checkpoints per turn, in the working tree, not per commit

Status: accepted (2026-11-05). Applies to the turn checkpoints in `DESIGN.md`
("Turn checkpoints: what a turn changed").

## Context

Axel records what every agent turn changed: a hidden commit of the working tree
before and after the turn, kept under `refs/axel/checkpoints/<thread>/<session>/
<ordinal>/{before,after}`, with the file list and both commit ids shared on the
turn envelope. The owner reads the diff, comments on lines, and undoes a turn
from that record, before anything is committed.

Entire (entire.io, the Entire CLI) is the closest shipped system and had the same
mechanism until mid-2026: a full working-tree snapshot per turn on a shadow branch
`entire/<commit>-<worktree>`, and `entire rewind <checkpoint>` to restore the tree.
Both were removed:

- `rewind` was deprecated with no replacement and removed; the dead restore
  path, about 1,800 lines, was deleted afterwards (PRs #1401, #1747, #2178).
- Shadow branches were removed in PR #2692, for runaway local storage and because
  deleting them at the wrong moment made uncondensed work unreachable. With
  about 95 sessions their commit hooks took around 16 s, because every commit
  resolved and condensed shadow branches per session.
- Many loose objects let `git gc --auto` corrupt a worktree's index
  (their known limitation ENT-241).

What Entire kept is a different unit: a checkpoint per *commit*. Agent hooks keep
pending work in `.git/entire-sessions/<id>.json` (step count, touched files, blob
hashes) and a local transcript copy; `prepare-commit-msg` stamps an
`Entire-Checkpoint: <ulid>` trailer; `post-commit` writes the transcript,
prompts and metadata under `refs/entire/checkpoints/<shard>/<ulid>` (one ref per
checkpoint, no contention); `pre-push` ships them to one elected remote. Elaborate
rules carry the trailer through squash, amend and reset. The product question it
answers is "why does this code exist", for a team, after the fact.

## Decision

Keep checkpoints per turn, in the working tree, local to the environment that ran
the turn. Don't move to commit-time checkpoints.

The two designs answer different questions. Entire's unit is right for provenance
of committed code. Ours has to show, review and undo an agent's work in the minutes
after it happened, when no commit exists yet, which needs the tree at turn
boundaries, exactly what Entire stopped recording. They dropped undo rather than
fix it; we consider undo the point.

What already keeps us clear of their failure modes:

- Nothing runs on `git commit`. Capture happens at turn start and end, with a
  time budget (10 s in the host, 6 s in a hook); past it the turn has no
  checkpoint and nothing else changes.
- Refs, not branches: no entries in `git branch`, nothing to clean up by name.
- Retention from day one: `axel gc` prunes archived threads after 7 days and
  keeps each live session's latest 20 turns past 30 days.
- Checkpoints never leave the owner's machines; the only transport is a fetch
  from a host the owner runs. Transcripts are not part of them.
- A snapshot shares unchanged blobs with HEAD: its cost is the changed files
  plus one tree object per touched directory. A clean tree is HEAD itself and
  an unchanged tree reuses the previous snapshot.

## Consequences

- Checkpoints are per environment. A remote participant sees the file list and
  totals from the envelope; the diff needs the objects, fetched from the host.
- A large file that is not ignored makes every snapshot slow; the host says so
  once and points at `.gitignore`.
- Loose objects accumulate between prunes (see improvement 3 below).
- Git history rewriting can't orphan a checkpoint: they are keyed by thread,
  session and turn, never by commit. None of Entire's trailer-inheritance
  machinery is needed.

## Possible future improvements (from Entire's design)

1. **Blob hashes per touched file on the turn envelope.** One `git hash-object`
   per file at `after`. Makes "does this file still hold the agent's version" a
   lookup, which the undo conflict check and the review pane can use without
   the objects, and remote participants can reason about without a fetch.
2. **Link commits to turns.** Compare HEAD at `before` and `after` and record
   the new commits on the turn (`commits: [sha]`). Optionally stamp an
   `Axel-Turn: <thread>/<session>/<ordinal>` trailer when the managed runtime
   makes the commit, so `git log` can point back at the thread. This is the
   cheap half of Entire's value and we capture both ends already. Keep it
   one-way: a rewritten commit loses the link, the checkpoint stays.
3. **Pack after pruning.** `axel gc` should run `git gc --auto` (or `git repack
   -d`) after deleting refs, so loose objects from snapshots don't pile up and
   can't trip auto-gc at a bad moment.
4. **Sweep orphaned turn marks.** A Claude pane session killed mid-turn leaves
   its `turn_marks` row open forever. Close marks whose session has ended, in
   `axel gc` or the reconciler, the way Entire's session-start zombie sweep
   finalises dead sessions.
5. **Retention settings** in `AXEL.md` (`checkpoints: keep_days, keep_turns,
   enabled`) instead of constants.
6. **Codex pane sessions.** Codex has only an end-of-turn notify; `before`
   would be the previous turn's `after` (or a snapshot at session start), with
   `between_turns` saying so.

What not to copy: pushing transcripts to the code remote (Entire's docs call
redaction best effort and note a public repository makes them public), hooks on
`git commit`, and commit-rewrite trailer rules.

## References

- Entire CLI: https://github.com/entireio/cli
- Sessions and checkpoints (architecture): https://github.com/entireio/cli/blob/main/docs/architecture/sessions-and-checkpoints.md
- Commit hook performance analysis: https://github.com/entireio/cli/blob/main/docs/architecture/commit-hook-perf-analysis.md
- Known limitations: https://github.com/entireio/cli/blob/main/docs/KNOWN_LIMITATIONS.md
- Session storage: https://docs.entire.com/guides/checkpoints/checkpoint-storage.md
- PR #2692, remove shadow branches: https://github.com/entireio/cli/pull/2692
- PR #1401, deprecate rewind: https://github.com/entireio/cli/pull/1401
