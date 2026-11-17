# ADR 0012: One `/skill` syntax for every agent, loaded by each harness as its own

Status: accepted (2026-11-17).

## Context

The owner had to remember which skills were installed to name one in a
prompt. Skills live in several places: the directories each harness reads by
itself (`.claude/skills`, `.agents/skills`, `~/.codex/skills`) and Axel's own
(`~/.config/axel/skills`). No harness reads all of them, and they name a skill
differently: Claude Code `/name`, Codex `$name`. A first design listed only the
target agent's skills, spelled its way; switching lists and syntax with the
target proved clunky. A second sent any agent any skill by pointing it at the
skill's file, which loses what a harness does with a skill it loads itself
(its base directory and scripts, its allowed tools, Claude Code's `Skill` tool,
Codex's skill input).

Both harnesses turned out to load a skill mid-session, without a restart and
without writing into the project (checked against Claude Code 2.1.326 and
Codex 0.169): Claude Code reads `.claude/skills` in a directory given with
`--add-dir`, and its `reload_skills` control request rereads it, in time for
the next prompt (without it a new skill shows a turn late); Codex's app-server
takes extra skill roots (`skills/extraRoots/set`), rereads them with
`skills/list` `forceReload`, and accepts a loaded skill as a turn input.

## Decision

The composer has one syntax and one list: `/` at the start of a word, Tab,
and every skill any harness or Axel has installed for the thread's projects
and the user, each name once, project skills first.

The session host finds the installed skills a message names and hands them
to the adapter with the prompt (`_meta.axel.skills`). The adapter, which
already speaks its harness's protocol, makes each one the harness's own: it
links the skill into a directory of the session's, outside the project, and
has the harness reread its skills; Codex also gets each as a skill input,
spelled `$name`. Only what that fails for, or an adapter that doesn't take
skills (any ACP agent but Axel's own, by capability as
[ADR 0007](0007-capabilities-not-harness-names.md) asks), is pointed at the
skill's file instead. The thread keeps the message as written.

## Consequences

Any agent can use any installed skill as if its harness had it all along,
and the composer doesn't depend on who the message goes to. Nothing is
written to the project or the user's skill directories; the session's links
go when the session does. Claude Code runs a skill as a command only when the
prompt starts with it, and Axel's catch-up comes first, so mid-session Claude
loads the skill through its `Skill` tool rather than as a command with
arguments. Skills a harness finds elsewhere (plugin skills) aren't listed yet;
the commands a live session reports could fill that in.
