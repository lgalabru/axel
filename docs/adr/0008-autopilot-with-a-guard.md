# ADR 0008: Autopilot by default, with a guard hook for the irreversible

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Autopilot: no
babysitting".

## Context

Babysitting permission prompts is the main cost of running several agents at
once. t3code gets "only ask me about design decisions" by running Claude with
`bypassPermissions` and surfacing `AskUserQuestion` as a user-input request
(`RESEARCH.md` "t3code"). Bypassing permissions with nothing in their place
leaves `git push --force` and `rm -rf` one bad turn away.

## Decision

Each agent has a runtime mode: `autopilot` (default), `edits`, `supervised`
or `plan`, mapped onto each harness's own flags. In autopilot no tool
permission is ever asked, but a `PreToolUse` guard (`axel hook claude`,
`shell_deny_reason` for the managed runtime) denies a short list of
irreversible actions: force pushes, `git reset --hard`, `rm -rf` outside the
workspace, `git clean -f`, pushes to protected branches, and the agent's
`disallowed_tools`. Design questions still reach the owner: `AskUserQuestion`
is kept on and routed to the inbox as a `question` the owner answers. Runtime
mode is separate from interaction mode (`build` | `plan`).

## Consequences

- An agent can run unattended for hours, including on a host where nobody can
  answer a prompt.
- The guard is a denylist, not a sandbox: it stops the obvious disasters and
  nothing more. Undo (ADR 0001) is the second line.
- Owners who want prompts choose `supervised`; the inbox shows them.
- A denied call is visible on the thread as a note, so the agent's "I
  couldn't" is explained.
