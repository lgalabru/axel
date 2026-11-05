---
name: axel
description: >
  Install, set up and use Axel (axel.md), the agent orchestrator for the Rex
  terminal: named agents with their own keys, durable threads, agents working
  together, agents on remote hosts over Tailscale, AXEL.md for project context
  and skills, per-turn checkpoints to review and undo an agent's work. Use it
  when someone asks to install or configure Axel, create agents, start or
  follow threads, let agents message each other, run an agent on another
  machine, review or undo what an agent changed, or asks what Axel is and how
  to get the most out of it.
license: Apache-2.0
compatibility: macOS with Rex (https://www.superlogical.com/rex) and at least one coding agent CLI installed; Linux for the CLI only.
metadata:
  author: axel.md
  source: https://axel.md/.well-known/agent-skills/
  docs: https://axel.md/docs
---

# Axel

Axel is agent orchestration designed for Rex. The person types what they want done; a
named agent (Claude Code, Codex, OpenCode, Goose, Cursor and others, run as their real
CLIs) works on it in a durable thread, on this machine or another one on their tailnet.
Axel adds who each agent is (an Ed25519 key), what it may do, what it is working on, what
it changed, what it costs and where it runs.

This file is the short version. The `references/` files are the documentation pages,
generated from https://axel.md/docs at build time; read the one you need before acting.
`axel <command> --help` has every flag.

## Before anything

1. Check the machine with `axel doctor` if Axel is installed, else install it:
   `curl -fsSL https://axel.md/install | sh` (or `brew install scarce/taps/axel`).
2. Confirm Rex is installed and running, and at least one harness is installed and
   logged in. Never guess at a login: ask the person to log in and tell you.
3. The first `axel` runs setup and creates the first agent. Run it in a Rex terminal.

Stop and ask when a requirement is missing. Details: `references/installation.md`.

## Doing things

| The person wants to… | Do | Read first |
|---|---|---|
| understand what Axel is | explain threads, named agents, Rex | `references/index.md`, `references/why-rex.md` |
| start working | `axel`, type the intent, Enter | `references/quick-start.md`, `references/keys.md` |
| create or change an agent | `axel agent new NAME HARNESS[/MODEL] --skills …`, `axel agent edit …` | `references/agents.md` |
| start, follow, resume a thread | `axel thread new --agent NAME "…"`, `resume`, `spawn`, `done` | `references/threads.md`, `references/cli.md` |
| have agents work together | `axel send "@opus …"`, `axel agent edit terra --allow opus` | `references/messaging.md` |
| set what an agent may do or spend | `--runtime-mode`, `--daily-usd`, the guard | `references/policies.md` |
| run an agent on another machine | `axel host add mini`, `@terra@mini`, `--host` | `references/remote-hosts.md` |
| set up a repo for agents | write `AXEL.md`, skills in its frontmatter, commit `AXEL.lock` | `references/axel-md.md` |
| see or undo what a turn changed | the change line, `axel thread diff`, `axel thread undo` | `references/tips.md` |
| report status from a script | `axel status working\|blocked\|done "msg"` | `references/agent-toolkit.md` |
| get more out of it | the habits | `references/tips.md` |

## Rules

- Agents cannot create agents or change policies; only the owner does. Don't try to
  work around a refusal: every refusal names the command that would allow it. Pass it on.
- Autopilot is the default and the guard refuses irreversible commands. Don't switch an
  agent to `supervised` unless the person asks for prompts.
- Prefer the managed runtime (built in for Claude Code and Codex). A harness without an
  ACP adapter runs in a plain pane and can't be messaged.
- Thread and session ids accept any unique prefix.
- After updating Axel, processes started earlier keep the old binary until restarted.

## Where things live

`~/.axel/axel.db` (the event log), `~/.axel/logs/`, `~/.axel/worktrees/`,
`~/.axel/scratch/`, `~/.axel/reviews/`; personal skills in `~/.config/axel/skills`; in a
repo `AXEL.md`, `AXEL.lock` and hidden refs under `refs/axel/checkpoints/`. Source:
https://github.com/scarce/axel.
