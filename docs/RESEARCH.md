# Research notes (2026-10-31, Entire added 2026-11-05)

Condensed findings from read-only surveys; conclusions that changed a choice are in
`adr/`. Raw per-method Rex schemas were
captured during research; re-derive with `rex api describe METHOD --json`.

## Rex (Superlogical) — Beta 0.1.0, API revision 7

**No embedded MCP.** The binary and app contain no MCP server. What exists:
- A read-only *Community MCP* at superlogical.com (docs + changelog, Discord OAuth).
- An agent skill + SessionStart hook (`rex skill setup`) that teaches agents the CLI.
- The real programmatic surface: a WebSocket control protocol `rex.control.v1` at
  `GET /api/control` on the Unix socket in `REX_SERVER`, self-documented at
  `GET /api/help` and `GET /llms.txt`. Hello → welcome → request/response + events.
  Also `rex.data.terminal.v1` on `/api/data` streaming raw PTY bytes + Ghostty snapshot.
- A Go SDK path (`go.superlogical.com/rex/sdk/...`) compiled in but not published.

**Blocks.** Only two creators today: `com.superlogical.terminal` (flavor `.shell`,
options `command[]`, `shell`("none"), `cwd`, `size`, `theme`, `initial_input`,
`exit{on_completion}`) and `com.superlogical.onboarding`. No web/markdown block.
An undocumented block-plugin system exists (`rex server run --block-dir`). So Axel
renders as a TUI in a terminal block.

**Floating panes: yes.** `session.new_layer {session_id, window_id?, bounds:[x,y,w,h]
fractions, layout:{block:{flavor,options,label}}, focus}` → `{layer_id, block_ids}`.
`session.float_block`, `resize_layer`, `raise_layer`, `lower_layer`, `close_layer`.
Layers appear in `session.view` as `kind:"floating"`. Client action `pane.float`.

**Status: OSC 7501** (rev 0.4). `ESC ] 7501 ; state=S[:id=..][:kind=..][:progress=N]
[:app=..][:title=b64][:msg=b64] ESC \`. States `idle|working|done|blocked|error|clear`,
kinds `permission|question|auth`. Read via block method `program_status`; events
`program_status_changed/removed`. Rex shows indicators in tab headers and the session
picker; a unified inbox is on their roadmap. Also parsed: OSC 9;4 progress, OSC 9/777
desktop notifications, OSC 133 prompt marks, OSC 7 pwd, OSC 52 clipboard, kitty graphics.

**Events.** `rex events -s S --json` streams `session_event{session_id, event{type,
payload}}`; block events nest as `block_event{block_id, creator_name, name, payload}`.
Session: window/block created/closed, labels, owner changes. Terminal: bell,
child_exited, title_changed, pwd_changed, program_status_*, desktop_notification.

**Lua.** `~/.config/rex/init.lua` (absent on this machine). `rex.action{name,title,run}`
→ command palette + `rex do NAME`. `rex.bind`, `rex.mode` (key modes), `rex.on` events
in scripts, `rex.exec`, `rex.server(label)` for multi-server scripts.

**Hosts.** `rex hosts add LABEL ENDPOINT`, `rex -S LABEL ...`. Every block has a
`host_id`. Remote servers via Tailscale (built-in tsnet) or plain HTTP. CLI-only today;
no block/session migration between hosts.

**Client actions** (Rex.app, needs Remote Control on): `session.select`,
`client.tab.goto`, `pane.split|float|zoom|close|resize`, `client.sidebar.activity.toggle`,
`client.theme.change`, modes. No action posts a toast or badge.

**Env in every Rex terminal:** `REX_SERVER, REX_SESSION, REX_BLOCK, REX_BIN_DIR`;
`TERM=xterm-rex`. Not implemented in CLI yet: `rex resize`, `rex block new`.

## t3code

**Correction:** execution never moves between machines. "T3 Code keeps execution in the
environment that owns the workspace." What feels like moving: clients re-attach over
failover routes (relay/loopback/lan/tailnet/ssh), `delegate_task` to a linked
environment (child thread there, durable follower here), provider switch mid-thread
via `ContextTransfer`/`ContextHandoff`, `t3_worktree_handoff`, restart continuation.

**Model worth keeping:** Environment = one server + machine + state, with an identity
independent of route. Project is environment-local. AppThread → Run → RunAttempt →
ExecutionNode tree, separate from ProviderThread/ProviderSession; app IDs primary,
provider IDs are refs with `strength`. Event log + command receipts (idempotent
`commandId`) + projections + durable effect outbox in one SQLite transaction;
snapshot+sequence streaming. Checkpoints as hidden git refs. Runtime mode (permissions)
separate from interaction mode (plan). Capability flags per provider and per server.

**Providers:** Claude via Agent SDK, Codex via `codex app-server` JSON-RPC, many via ACP,
Cursor SDK, OpenCode HTTP. No PTY scraping anywhere. Server exposes `/mcp` (tools
`t3_thread_launch|send|read|wait|...`, `delegate_task`) and typed WS RPC. `t3 thread`
CLI is already a minimal headless client.

## Buzz

Nostr relay + clients. Agents are secp256k1/BIP-340 keypairs; nothing uses Ed25519.
Inbox = NIP-01 events whose `p` tag is me. Pure, liftable modules:
`buzz-sdk/src/nip_oa.rs` (owner→agent attestation, conditions mini-language),
`buzz-sdk/src/mentions.rs` (`@name`/URI → `p` tags, cap 50),
`buzz-core/src/{verification,nip10,filter,kind}.rs`, `buzz-persona` (`.persona.md`,
`name_pool`, `RespondTo`). Keystore pattern: one keyring JSON blob, `agent:<pubkey>`
entries, write-and-verify, `0600` file fallback, refuse to spawn without key, zeroize env
key after parse. Agents run through `buzz-acp` (relay WS → ACP stdio pool), not PTYs.

## Axel v1

tmux launcher + Swift app. Worth porting: `AXEL.md` frontmatter config with `agents:`
(`id,name,harness,model,api,skills,prompt,pubkey`), skill resolution and per-harness
install drivers, `axel acp` stdio relay with env mapping, `git.rs` worktree isolation,
`ClaudeCommand` builder. Drop: tmux, axum hook server, OTEL scraping, `send-keys`
replies, Codex bell scraping, trust-prompt keystrokes. Root cause of the todo/inbox
pain: task↔terminal identity was inferred from scraping and shared hook files.

## Entire (entire.io CLI, 2026-11-05)

Provenance for teams: a checkpoint per *commit*, written by `post-commit` under
`refs/entire/checkpoints/<shard>/<ulid>` (transcript, prompts, metadata), linked by an
`Entire-Checkpoint` trailer, pushed by `pre-push` to one elected remote. Pending work
between commits lives in `.git/entire-sessions/<id>.json` (touched files + blob hashes).
They *had* per-turn working-tree snapshots on shadow branches and `entire rewind`, and
removed both in 2026 (storage growth, 16 s commit hooks, cleanup losing work, no
replacement for rewind). Different question from ours; see
`adr/0001-turn-checkpoints-not-commit-checkpoints.md` for why we keep turn checkpoints
and what to borrow (blob hashes, commit→turn links, packing after prune, zombie sweep).

