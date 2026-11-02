<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/axel-banner-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/axel-banner-light.png">
  <img alt="Axel the axolotl. Featured on Product Hunt, #3 Product of the Day" src="docs/assets/axel-banner-dark.png" width="100%">
</picture>

# Axel

**Agent orchestration, designed for [Rex](https://www.superlogical.com/rex).**
Website: [axel.md](https://axel.md) · docs: [docs.axel.md](https://docs.axel.md).

A year of agent apps taught us what works. Axel keeps those ideas and brings them back to the
terminal:

- **Threads**, from the Codex app and T3 Code. The unit of work is a thread, not a chat tab:
  an intent and everything done about it, outliving the agent's process, the pane and the
  machine. Run several at once, resume tomorrow, settle when done.
- **Named agents**, from Buzz and skills.sh. A team of specialists instead of one assistant
  that knows everything: `atlas` builds with a few skills for your stack, `nova` reviews with
  another model and a review skill. Each has its own Ed25519 key, permissions and budget, and
  loads only its own skills.
- **The terminal**, from Rex. Agent apps rebuilt the terminal because it couldn't keep work
  alive, show who's waiting on you, or reach your other machines. Rex can: durable sessions,
  the Program Status Protocol (`OSC 7501`), Tailscale built in, and a server API programs can
  drive. Axel is just a program in a Rex pane, and your Mac mini on the tailnet is a host
  (`@atlas@mini`). See [Why Rex](https://docs.axel.md/why-rex).

```
$ axel            # inside Rex: the launcher floats over the window.
                  #   why not today? — the prompt. Type what you want, Enter starts it
                  #                    with the agent shown at the right (the last one
                  #                    used, or the one an @name picks), in place, here.
                  #                    Tab opens the full form (projects, agent, worktree).
                  #   threads        — recent threads below it: ↓ to get there, Enter opens
                  #                    one, Ctrl+S settles, Ctrl+X archives, Ctrl+D deletes.
                  #   usage          — a small HUD of every agent: state, tokens, sparkline.
$ axel            # again: closes them (toggle)
```

Threads are the primitive: an intent, attached to any number of projects, worked by many
agent sessions over time on any machine, shared with other people. Every change is a signed
event, so replicas merge. Agents are named Ed25519 identities with an inbox. Their status flows back through
Claude Code hooks, Codex notify, ACP, or `axel status`, never by scraping a terminal,
and is mirrored to Rex through OSC 7501 so Rex's tab indicators agree with Axel.

See `docs/DESIGN.md`. Install: `just install` (release build, symlinked into `~/.local/bin` and `~/.cargo/bin`).

## Commands

| | |
|---|---|
| `axel` | floating launcher / inbox (inline when not in Rex) |
| `axel ui [--screen launch\|inbox]` | what the floating pane runs |
| `axel agent new NAME [--harness claude\|codex\|opencode] [--model M] [--skills a,b\|*]` | create an agent (key, attestation, record) |
| `axel agent ls\|show\|archive` | |
| `axel thread new "intent" --agent NAME [--project DIR]... [--scratch] [--worktree [BRANCH]]` | thread + first session |
| `axel thread spawn THREAD --agent NAME` | another agent on the same thread |
| `axel thread resume THREAD [--agent NAME]` | new session continuing the last harness conversation |
| `axel thread attach THREAD --project DIR` | |
| `axel thread open THREAD` | the thread view: conversation, folded tool calls, composer |
| `axel thread ls [--all]`, `show`, `stop`, `done`, `archive` | |
| `axel session host SESSION` | hosts a managed (ACP) session; its pane is the raw log |
| `axel session ls [--all]`, `axel session stop SESSION` | |
| `axel log SESSION [-f]` | tail a managed session's raw frames |
| `axel send "@atlas please review the diff"` | signed message into an agent's inbox |
| `axel inbox [--agent NAME]` | |
| `axel status working\|blocked\|done\|… "msg"` | status from scripts (reads `$AXEL_THREAD`) |
| `axel hook claude\|codex` | hook sink, installed automatically at launch |
| `axel doctor` | |

## Runtimes

- **managed** (default when the harness's ACP adapter is on PATH): Axel drives the agent over
  the Agent Client Protocol. You work in the **thread view**: your messages, the agent's prose,
  questions inline; tool bursts fold into one line (Space expands); `l` jumps to the host pane,
  which prints the raw frames. Permissions are decided by Axel's policy, never by a dialog.
  Adapters: `npm i -g @zed-industries/claude-code-acp` (Claude), `codex-acp` (Codex), `goose acp`.
- **pane**: the harness's own CLI in a Rex pane; status via hooks.

## Autopilot

Agents default to `autopilot`: Claude Code runs with `--permission-mode bypassPermissions`
(Codex with `--full-auto`) and Axel's guard hook still denies irreversible shell commands.
Axel pre-accepts Claude Code's folder-trust dialog for the project you picked. Claude Code's
*bypass permissions* warning is yours to accept once per machine: either press
"Yes, I accept" the first time, or set it in `~/.claude/settings.json`:

```json
{ "skipDangerousModePermissionPrompt": true }
```

(The older `bypassPermissionsModeAccepted` key in `~/.claude.json` is only read by a
one-time migration into that setting.)

## Rex key binding

Append `docs/rex-init.lua` to `~/.config/rex/init.lua`, then `rex config reload`.
