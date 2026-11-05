<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/axel-banner-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/axel-banner-light.png">
    <img alt="Axel the axolotl. Featured on Product Hunt, #3 Product of the Day" src="docs/assets/axel-banner-dark.png" width="100%">
  </picture>
</div>

<p align="center">
  <a href="https://github.com/scarce/axel/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/scarce/axel?color=a64dff"></a>
  <a href="https://www.superlogical.com/rex"><img alt="Built for Rex" src="https://img.shields.io/badge/built%20for-Rex-f58a62"></a>
  <img alt="macOS" src="https://img.shields.io/badge/platform-macOS-black">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-green"></a>
</p>

<p align="center">
  <b>Agent orchestration, designed for <a href="https://www.superlogical.com/rex">Rex</a>.<br>
  Threads and named agents, back in the terminal.</b>
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="https://axel.md/docs/quick-start">Quick start</a> ·
  <a href="https://axel.md/docs">Docs</a> ·
  <a href="https://axel.md">axel.md</a>
</p>

---

```sh
# Two agents: one builds, one reviews with another model
axel agent new terra codex
axel agent new opus claude/opus

# A thread: terra does the work, then hands the diff to opus
axel thread new --agent terra "add retries to the webhook sender, then ask @opus for a review"
```

Or run `axel` in Rex and type the same thing into the launcher that floats over your window.
The thread keeps going when you close the laptop, and you can pick it up from any machine.

## Key features

### 🧵 Threads, not chat tabs

A thread is an intent and everything done about it. It outlives the agent's process, the
pane and the machine: run several at once, resume tomorrow, settle it when it's done. Every
change is a signed event, so copies on other machines merge.

### 🪪 Named agents

A team of specialists instead of one assistant that knows everything. `terra` builds with a
few skills for your stack; `opus` reviews with another model. Each has its own Ed25519 key,
permissions, budget and inbox, and loads only its own skills.

```sh
axel agent new opus claude/opus --skills review
axel send "@opus please review the diff"
```

### 🖥️ The terminal, fixed by Rex

Agent apps rebuilt the terminal because it couldn't keep work alive, show who's waiting on
you, reach your other machines, or even scroll. Rex fixes all of that, so Axel is just a
program in a pane: threads stream as plain output and scroll like anything else, and status
shows up in Rex's tabs. [Why Rex →](https://axel.md/docs/why-rex)

### 🤖 Claude Code, Codex, OpenCode, Goose, Cursor and more

Bring the harness you already use, and pick its model in the same word: `claude/opus`,
`codex`, `goose`, `cursor/sonnet`. Axel drives Claude Code and Codex over the Agent Client
Protocol with built-in adapters (no Node), and OpenCode, Goose, Cursor, Pi, Oh My Pi, Devin,
Grok Build, Kimi Code, Amp, Hermes Agent and OpenClaw through their own ACP modes. Any other
harness runs in its own pane. Status comes
from hooks and the protocol, never from scraping a screen.

### 🌐 Your other machines

Your Mac mini on the tailnet is a host. Agents run there, and their keys never leave home:
the host gets a short-lived grant per thread. [Remote hosts →](https://axel.md/docs/remote-hosts)

```sh
axel host add mini
```

### 📜 One AXEL.md for every harness

The project's context and the skills its agents use, in one file that `CLAUDE.md` and
`AGENTS.md` point at. Skills can be local folders or links from skills.sh and GitHub, pinned
in `AXEL.lock`. [The AXEL.md file →](https://axel.md/docs/axel-md)

```yaml
skills:
  - ./skills
  - https://skills.sh/vercel-labs/skills/find-skills
```

### 🛡️ Autopilot, with a guard

Agents work without asking by default. Axel's guard still refuses irreversible commands,
and a budget caps what each agent spends. Want a say? Give an agent a stricter mode.
[Permissions and budgets →](https://axel.md/docs/policies)

## Install

```sh
curl -fsSL https://axel.md/install | sh
```

or with Homebrew:

```sh
brew install scarce/taps/axel
```

Then run `axel` in a Rex terminal. The first run checks your machine and helps you create
your first agent. Every commit on `main` that passes CI is a release, so run the script
again or `brew upgrade` to update.

You'll need [Rex](https://www.superlogical.com/rex) and a harness:
[Claude Code](https://claude.ai/code), [Codex](https://openai.com/codex),
[OpenCode](https://opencode.ai), [Goose](https://goose-docs.ai), [Cursor](https://cursor.com/cli) or
[another](https://axel.md/docs/installation#harnesses). Details in the [installation guide](https://axel.md/docs/installation).

## Documentation

- [Quick start](https://axel.md/docs/quick-start): your first agent and thread
- [Threads](https://axel.md/docs/threads) and [agents](https://axel.md/docs/agents)
- [Agents working together](https://axel.md/docs/messaging)
- [Keyboard](https://axel.md/docs/keys) and [command line](https://axel.md/docs/cli)
- For agents and LLMs: [llms.txt](https://axel.md/llms.txt), every page in
  [llms-full.txt](https://axel.md/llms-full.txt), and an Agent Skill that installs and runs
  Axel for you: `npx skills add https://axel.md`

## Development

```sh
just install    # release build, linked into ~/.local/bin
cargo test --workspace
```

The design lives in [`docs/DESIGN.md`](docs/DESIGN.md), the wire formats in
[`docs/CONTRACTS.md`](docs/CONTRACTS.md), and the website in [`web/`](web).

## License

[Apache-2.0](LICENSE)
