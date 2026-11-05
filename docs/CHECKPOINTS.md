# Turn checkpoints: see, review and undo what an agent changed

Status: draft. Builds on *Autopilot* and *Managed runtime* in `DESIGN.md`.

## The problem

Autopilot only works if what happens afterwards is cheap. You don't watch an agent's
tools run. You come back to a finished turn and need three answers fast:

1. **What changed?** Which files, how much.
2. **Is it right?** Read the change, and say what's wrong with it.
3. **If not, make it go away.**

Today Axel shows the conversation, the tool calls and the tokens, but for those three
answers you leave for a shell: `git status`, `git diff`, `git stash`, `git checkout --`.
That's worse on a thread with several agents in one tree, where `git diff` mixes
everyone's work since the last commit and says nothing about which turn did what.

## What you get

Every turn of every session in a git workspace leaves a **checkpoint**: a hidden commit of
the tree before the turn and another after it. From those:

- **A change line under each turn** in the thread view: `3 files  +42 −7`. Nothing when
  the turn changed nothing.
- **The turn's diff**, one click (or `axel thread diff`) away: exactly what changed during
  that turn, file by file, syntax-highlighted, in a pane beside the conversation.
- **Undo turn**: take that turn's change back out of the tree, keeping everything done
  since, as long as it doesn't overlap.
- **Review comments**: in the diff, mark lines and write a note. Notes go to the agent as
  its next prompt, each with the path, the line range and the hunk attached, so "this
  is wrong" points at the exact code.

## Capture

`snapshot_commit` (`crates/axel/src/cmd/host.rs`) already does what's needed. It commits the
working tree (tracked and untracked files, not ignored ones) through a temporary
index, on top of HEAD, unsigned, without touching the user's index, branches or HEAD. It
returns HEAD itself when the tree is clean. The remote handoff already uses it.

**Managed sessions** (`axel-acp/src/host.rs`):

- `start_turn`: snapshot the session's cwd → `before`. Synchronous, before the prompt
  goes to the harness, so the agent's first edit is never in `before`.
- After `TurnEnd`: snapshot again → `after`. Then diff `before`'s tree against
  `after`'s tree (`git diff --numstat -z` with rename detection) to get the file list.

**Pane sessions** (the native CLI in a Rex pane):

- Claude: `UserPromptSubmit` takes `before`, `Stop` takes `after`. Both hooks already
  run through `axel hook claude`.
- Codex has only `notify`, at the end of a turn. `before` is the previous turn's
  `after`, or a snapshot taken when the session started. Edits made between turns
  are then counted in the next turn; the change line says so (`incl. changes between
  turns`) when the tree at turn start didn't match the last `after`.

**Refs.** Each snapshot is kept alive under
`refs/axel/checkpoints/<thread>/<session>/<ordinal>/{before,after}` in the workspace's
repository (for a worktree, the shared repository). The refs never show up in
`git branch` or `git log --all` from the usual commands, and pushes never send them, since
nothing pushes `refs/axel/*`.

**Cost.** A snapshot costs about as much as `git add -A` on a temporary index: it re-hashes
modified and untracked files, so it's fast on a normal tree. The worst case is a huge
untracked file that isn't ignored. If a snapshot takes longer than 2 s, the session logs
it once and suggests the file to add to `.gitignore`. It never blocks a turn for more
than 10 s: past that, the checkpoint is marked `skipped` for that turn.

**Nothing to capture.** For a scratch thread, a project that isn't a git repository, or a
repository without a first commit, there are no checkpoints. The turn keeps today's
`files_edited` count.

## Data

The kind 40 `turn` envelope gains an optional `changes` field. Old readers ignore it.

```rust
pub struct TurnSummary {
    // existing fields…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<TurnChanges>,
}

pub struct TurnChanges {
    pub before: String,           // commit oid
    pub after: String,            // commit oid; == before when nothing changed
    pub files: Vec<FileChange>,   // at most 200; `truncated` says there were more
    pub truncated: bool,
    pub between_turns: bool,      // `before` didn't match the previous turn's `after`
}

pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>, // renames
    pub status: ChangeStatus,     // Added | Modified | Deleted | Renamed
    pub added: Option<i64>,       // None for binary files
    pub removed: Option<i64>,
    pub authored: bool,           // this session's edit tools touched it during the turn
}
```

`ChangeStatus` is an enum, converted at the JSON edge.

Only oids and the file list travel in the signed log. The diff text stays in the local
repository. That fits the rule that conversation items stay on the machine that ran them.

**Attribution (`authored`).** With several agents in one in-place workspace, the tree
diff for a turn picks up whatever anyone changed during it. The managed host already
counts this turn's file edits from ACP tool calls, which carry locations. Keep those
paths, and mark a changed file `authored` when one of this session's edit tools touched
it during the turn. Files changed by shell commands (formatters, codegen) aren't
authored. The diff pane lists unauthored files apart, under *also changed during this
turn*, so you can tell the agent's work from its neighbours'. Pane sessions get
`authored` from Claude's `PostToolUse` for `Write`/`Edit`/`MultiEdit`; Codex leaves it
false for every file.

## Showing it

**The change line.** Under each turn end in the thread view (`axel-tui/src/thread.rs`,
where `TurnEnd` items render), dim: `3 files  +42 −7`, or `3 files (+1 not this
agent)  +42 −7`. It's an `axel-links://` link, like a run of tool calls is today, and it
opens the diff pane.

**The diff pane.** `axel thread diff THREAD [--session S] [--turn N]` opens a floating
Rex pane where the tool-calls pane goes (`links.rs`). Without `--turn`, it shows the
latest turn that changed something.

```
╭ lyra · turn 4 · 3 files +42 −7 ──────────────────────────╮
│ M crates/axel-tui/src/view/usage_view.rs        +38 −5   │
│ M crates/axel-tui/src/model.rs                   +3 −1   │
│ A docs/CHECKPOINTS.md                            +1      │
│ ─ also changed during this turn ─                         │
│ M Cargo.lock                                     +0 −1   │
├──────────────────────────────────────────────────────────┤
│ @@ fn columns(width: usize, name: usize) @@               │
│ -    let spark = width - used + SPARK_MIN;                │
│ +    let per = SPARK_MINUTES.div_ceil(width - used + 6);  │
│ …                                                         │
╰ ↑↓ file  j/k line  v mark  c comment  u undo turn  ⏎ send ╯
```

The file list sits on top and the diff of the selected file below it. Hunks are
syntax-highlighted with the theme the conversation already uses. Scrolling is the
pane's own. `[` and `]` step to the previous and next turn of the session.

`axel thread diff --stat` prints the file list and exits, for scripts and for agents.

## Undo turn

Undo takes one turn's change back out of the tree without disturbing work done since:

1. Snapshot the current tree → `undo-base`, kept under
   `refs/axel/checkpoints/<thread>/undo/<n>`, so the undo can be undone.
2. Apply the reverse of `before..after`, limited to the turn's authored files, to the
   working tree only (`git apply -R --3way`, never touching the index).
3. If a hunk doesn't apply cleanly, because later work overlaps it, stop before writing
   anything, list the conflicting files, and offer **restore the tree to before this
   turn** instead. That one rewrites every authored file to its `before` content and is
   confirmed separately.
4. Tell the agent. Post a note into the session, delivered as the start of its next
   prompt: *"The owner undid your turn 4: crates/…/usage_view.rs and 2 more are back to
   how they were before it."* Otherwise the agent's picture of the tree is wrong and it
   builds on code that's gone.

It's confirmed in the composer (`undo turn 4 of lyra: 3 files? y/n`) and only the owner
can do it. Agents never get an undo tool. The guard's `git reset --hard` rule isn't
involved: undo is a scoped patch with its own snapshot, not a reset.

Undo isn't offered while the session is mid-turn: the agent might be editing those
same files.

## Review comments

In the diff pane, `v` starts a mark on the current line, `j`/`k` extend it, and `c`
opens a one-line note under it. Notes pile up as drafts, shown inline. `⏎` sends every
draft as **one message** to the session, through the composer, where you can add to it
before it goes:

````
Review of your turn 4:

```review path=crates/axel-tui/src/view/usage_view.rs lines=85-91 side=new
    let spark = if chosen.contains(&Col::Spark) {
        let per = SPARK_MINUTES.div_ceil(width - used + SPARK_MIN);
        SPARK_MINUTES.div_ceil(per)
```
This can still leave the chart narrower than its label; clamp to 9.
````

One fenced block per note: path, line range, which side of the diff (`old` for removed
lines, `new` otherwise), the marked lines, then the note. Agents read it as plain text,
so it works on every harness. In the thread view, the message renders as a list of
`path:lines` chips with the notes, and each chip opens the diff at that spot.

Drafts are kept in the local store per session, so closing the pane doesn't lose them.

## Remote hosts and other participants

The checkpoint lives in the repository of the environment that ran the turn. Other
participants see the change line from the envelope. Opening the diff from elsewhere
asks that environment for `before..after` over the connection the remote host already
uses for snapshots, and caches the objects locally. Until that exists (phase 4), the
pane shows the file list and says where the diff lives.

Undo always runs on the environment that owns the workspace.

## Retention

Checkpoint refs keep objects alive, so they need pruning:

- When a thread is archived, its refs go after 7 days.
- Live threads keep every turn's checkpoints for 30 days, then only the latest 20 per
  session.
- `axel gc` (also run after archive) deletes expired refs. Git's own gc reclaims the
  objects.

Settings live in `AXEL.md` under `checkpoints:` (`keep_days`, `keep_turns`, `enabled`).

## Phases

1. **Capture and the change line.** Snapshots at turn start and end, refs, `changes` on
   the turn envelope, the line under each turn, `axel thread diff --stat`. Managed
   sessions first, then the Claude pane hooks.
2. **Diff pane.** File list, highlighted hunks, turn stepping, opened from the change
   line.
3. **Undo turn.** Reverse patch, undo snapshot, conflict fallback, the note to the agent.
4. **Review comments.** Marks, drafts, the fenced format, chips in the thread view.
5. **Across environments.** Fetching checkpoint objects from the owning environment, and
   retention and `axel gc`.

## Open questions

- **Pulls and merges during a turn.** If HEAD moves because of a pull, the tree diff
  includes everything the pull brought in. `authored` filters it out of the main list,
  but a pane Codex session has no `authored`. Should a snapshot taken after HEAD moved
  diff against `merge-base(before, after's parent)` instead?
- **Ignored files.** Build output and `.env` aren't captured, which is what we want, but
  then undo can't bring back an ignored file the agent deleted. Is that worth a warning
  in the diff pane when a turn deletes an ignored file?
- **Whole-thread view.** A diff of everything a thread changed, across turns and
  sessions (from the first `before` to the latest `after`), is one more query. Is it
  phase 2 or later?
