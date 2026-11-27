# Research notes

Condensed findings from read-only surveys; conclusions that changed a choice are in
`adr/`. Raw per-method Rex schemas were
captured during research; re-derive with `rex api describe METHOD --json`.

## Graph engineering (2026-11-10)

Read Arcos's [Graph Engineering article](https://x.com/0xarcos/article/2119068641740091716)
from the owner's 23-page PDF export. The article is dated 2026-11-07; the export
was created on 2026-11-10. The PDF snapshot has SHA-256
`2cafd22a358a36891dc0991c182803d6ae6f705b82ab72e0d03ccfb541736430`.
All pages were read; the printed checker and runner were also inspected visually.
Code lines are clipped at the right edge in parts of the export. This is a review
of that printed sample and its extracted text, not an executed reference repo or
an independent reproduction of the author's overnight results.

**Mechanism.** A splitter proposes units with file ownership and dependencies.
Fresh Claude sessions work in separate Git worktrees. `graph.py` dispatches ready
waves and merges accepted branches into `graph/integration`. `check.py` checks
scope and runs the repository's test command. A Stop hook feeds failures back to
one station. Outer retries return a structured failure packet to that unit.
Risk lanes hold some candidates for the owner. Failures append to `lessons.md`;
the planner is instructed to turn repeated lessons into future planning rules.
The owner performs the final merge. See PDF pp. 6-7 and 10-21.

**What the sample does not establish.** These are observations of the printed
code, not claims about unpublished production tooling:

| Source location | Observation | Consequence for Axel |
|---|---|---|
| `check.py`, p. 11 | Uses `git diff --name-only HEAD` plus untracked files | Compare against the assigned immutable base, including commits the worker made |
| `check.py`, pp. 11-12 | Executes checker/config from the worker worktree; allowed test files can still be edited | Run trusted verification outside worker control; a locked-file prompt is not independent enforcement |
| `waves()`, pp. 14-15 | Checks named dependencies and a nonempty unit-level `reads` string, but does not validate disjoint ownership | Validate each edge's artifact and overlapping paths before dispatch |
| `lane()`, pp. 14-15 | Classifies declared file strings, which can themselves be globs | Classify actual changed paths, then apply owner-controlled risk rules |
| Integration loop, pp. 15-16 | Merges green units but does not run a final combined test gate | Verify the exact combined candidate before final review |
| `sh()` and `drive()`, pp. 14-15 | Several worktree/commit/model-process results are unchecked; subprocesses have no deadline | Failed commands must affect run state; enforce time and resource limits |
| Run/ref naming, pp. 14-15 | Minute-based run directories and reusable `graph/integration` / `graph/<unit>` branches; no recovery journal | Use unique run IDs, immutable references, leases and crash reconciliation |
| Stop hook and outer loop, pp. 12-15 | Local hook retries are reset inside each outer attempt | Define one repair budget; nested limits are not three total repair cycles |
| `RED nothing changed`, p. 11 | An already satisfied task cannot pass without an edit | Permit a verified no-op rather than encourage unnecessary changes |
| `lessons.md` / splitter, pp. 14-15 and 20-21 | Lesson promotion depends on planner instructions, without a checked promotion record | Separate evidence, suggested planning lessons and owner-controlled policy |
| Run report, pp. 16-17 | Counts shipped integration units and held units, including an example with two owner touches | Keep partial completion and additional owner input explicit |
| Headline versus runner, pp. 1 and 14-16 | Claims twenty agents across four repos, while this runner operates from one root and configures four workers | Treat twenty-way and multi-repo operation as experiments, not demonstrated guarantees |

**External evidence checked on 2026-11-10.** Anthropic's
[Building effective agents](https://www.anthropic.com/research/building-effective-agents)
separates workflows from agents and describes orchestrator-worker and
evaluator-optimizer patterns. It supports using simple composable control flow;
it does not require discarding advisory model review.

Anthropic's [multi-agent research system](https://www.anthropic.com/research/multi-agent-research-system)
reports a 90.2% improvement over single-agent Opus 4 on an internal research eval,
and approximately 15 times chat token use. It also notes that coding often offers
less parallel work. Those are research-system results, not a coding speedup or
budget forecast for Axel.

Cognition's [Don't Build Multi-Agents](https://cognition.com/blog/dont-build-multi-agents)
warns about lost context and conflicting implicit decisions. Separate worktrees
address filesystem interference, not all semantic coordination. Preserve shared
requirements and interfaces, and make relevant decision traces retrievable.

The current [Claude Code Stop hook reference](https://code.claude.com/docs/en/hooks#stop)
supports blocking completion with exit 2 and describes an eight-consecutive-
continuation cap. That counter resets on a tool call and is configurable. It is
not an overnight run budget, and Axel should own acceptance outside a
harness-specific hook.

**What we take.** A deterministic controller, isolated worker contexts, named
input artifacts, executable acceptance gates, scoped retries, private integration
and an owner review of the final candidate. We do not take first-attempt green as
a proof of low risk, worktrees as a security sandbox, or a shared mutable rule file
as an automatic learning system.

The adaptation, proposed milestones and unanswered questions are in
[RFD 0002](rfd/0002-verified-agent-graphs.md). It remains a proposal.

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
`TERM=xterm-rex`. From 0.1.0-preview.23, new terminals no longer set `REX_SESSION`:
a command finds its session from `REX_BLOCK` (`rex whoami --json` answers
`session_id`), and any `-s` accepts a block id (issue #42). Not implemented in CLI yet: `rex resize`, `rex block new`.

### Rex 0.1.0-preview.22, API revision 19 (2026-11-22)

Studied from the server's own `GET /llms.txt` and `GET /api/help`, `block_creator.list`,
CLI help and strings in the binaries. What changed since revision 7, and what matters to us.
The plan built on it is [RFD 0003](rfd/0003-rex-native-axel.md).

- **Control connection.** `hello` carries `api_revision` and `api_min_revision`; the server
  picks the newest shared revision and translates messages for older clients. `welcome`
  adds `reconnect_token` (resume the same client within 60 s), `features`, and `carriers`
  (other transports, such as QUIC). `session.bootstrap{event_scope, attach_session_ids,
  data_tickets}` subscribes, lists and attaches in one call. Hello must stay under 512 bytes.
- **Principals.** Every client has a server-verified `principal`: `peer` (uid), `tailscale`
  (`user_id`, `login_name`, `node`, `tags`; any tag means a machine) or `anonymous`.
- **Client actions.** A client declares the actions it performs in `client.keymap{actions}`;
  others call them with `client.perform`, and the server names the requester. Rex.app
  offers `client.quick_look` (previews files and links on screen) among others.
- **Prompts (revision 17).** `rex.ui.input` and `rex.ui.pick` in a config-file action reach
  every connected app, including iOS, as `client_prompt_requested`; the first answer wins.
  Default timeout 300 s, at most 3600. Only inside an action's `run`. `rex do` waits for the
  answer. `client.set_script` actions are announced "from a later version of Rex".
- **Terminal.** New options `path_prepend`, `login`, `interactive`,
  `exit.quick_exit_threshold_ms`; `args` is deprecated for `command`. New methods
  `process`, `ports`, `list_dir`, `input_mode`, `download_begin`/`upload_begin` over
  `rex.data.bytes.v1`. New events `process_changed`, `ports_changed`, `progress_report`,
  `url_clicked`, `selection_copied`, `size_changed`, `clipboard_written`.
- **Layout.** `session.set_layer_state` minimizes and restores floating layers.
- **New creator.** `com.superlogical.tunnel` with flavors `dial` and `listen`, behind
  `rex forward`.
- **Block plugins.** `rex server run --block-dir DIR` starts executable plugins that advertise
  creators; `--block-plugin-reload` reloads them; `creator_changed` announces changes. The
  docs list "Block Plugins" with no page yet. There is no plugin registry anywhere.
- **Servers.** Login-server and shared-server modes for several accounts,
  `--tailscale-control-url` for Headscale, auth keys and ACL tags for unattended enrollment,
  and limits (`--max-sessions` 64, `--max-blocks` 1024, `--max-windows` 64, `--max-panes` 64).
- **Browser.** No web view in Rex.app or the server. The protocol has a `web` client platform
  and the server takes `--websocket-origin-pattern`, so a browser client looks planned.

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


## terminal-browser (zenbu-labs, 2026-11-26)

Chromium drawn inside a terminal pane. Read: its README, `cli/src/main.ts`,
`shared/src/terminal-socket.ts`. Not installed here, not run.

- **What it is.** Electron's offscreen rendering reads Chromium's pixels from
  the GPU, and only the changed regions go to the terminal as Kitty graphics
  protocol images. Input comes from the terminal's mouse and key reports; on
  macOS a background Swift helper adds smooth trackpad scrolling. MIT.
  Install with `brew install terminal-browser`.
- **Terminals.** The README lists ghostty, rex, kitty, iTerm2, wezterm, cmux,
  tmux, vscode, tty7, herdr and supacode as having built-in support. Windows
  works only under WSL.
- **CLI.** `terminal-browser open URL` runs in the foreground until the user
  closes it, then exits with the session's code (0 when closed normally).
  Other commands and flags:
  - `--split right|left|up|down` opens it in a new pane;
  - `--no-merge` keeps it out of a browser already running (also
    `TERMINAL_BROWSER_NO_MERGE=1`);
  - `--transparent`, `--allow-clipboard-read`, `--ssh=`;
  - `ls`, `action` (an agent-browser compatible CLI), `upgrade`.
- **What it dropped.** App mode, `--no-toolbar` and `--no-frame` were
  removed: "app mode was removed from terminal-browser". Its bar always shows.
- **Telemetry.** On by default; `DO_NOT_TRACK=1` or
  `terminal-browser config set telemetry.usage off` turns it off.
- **Terminal socket.** A terminal that wants deeper integration sets
  `TERMINAL_BROWSER_TERMINAL_SOCKET`, a Unix socket with one JSON request per
  connection. The methods are `hello`, `getCurrentPane`, `split`,
  `listPanes`, `neighbor`, `sendText`, `pasteKey` and `focusPane`. Rex could
  serve it; Axel doesn't need to.
- **Neighbours.** awrit (Chromium, Kitty graphics, the first of these),
  brow6el (CEF, sixel and Kitty graphics, "POC code quality"), Carbonyl
  (Chromium drawn as text, works over SSH).
- **What we take.** A web page as a pane, through `terminal-browser open
  --no-merge`, with the terminal view as the fallback (RFD 0007).
- **What we don't.** Its terminal socket, its agent CLI, and any reliance on
  merging into a shared browser.

## Rex 0.1.0-preview.25 (build 2154, 2026-11-26)

Installed by the app's updater beside the running `preview.22`, and run here
as a throwaway server (`--state-dir` in `/tmp`, `--config /dev/null`):
- The API is at revision 24, up from 19. New methods: `client.undo_close`,
  `session.transfer_window`, and the `com.superlogical.fs` block creator.
- Terminals gain `program_resume_records`. `OSC7577_TOKEN` is set in each
  pane, though not with `--restore=false`.
- Block creators: `terminal`, `tunnel`, `fs`, `onboarding`. No browser or web
  block. Executable block plugins load from `--block-dir`, but their protocol
  isn't documented.
- Program resume needs a trust rule (`rex.set("resume.trust.NAME", { command
  = {...} })`). Rex ships rules for `claude`, `codex` and `opencode`, and
  checks that the reporting process is the program the command starts.

## Ladybird (2026.47.7, 2026-11-26)

An independent browser engine (LibWeb, LibJS), BSD-2-Clause, alpha since
2026-11-23 on macOS and Linux. Read: its news and newsletters, the
repository's `Cargo.toml` and `Libraries/LibWebView/TerminalWebView.{h,cpp}`.
Run: the copy in `/Applications`.

- **Rust.** Since February 2026 parts of it are moving to Rust: LibJS first,
  then style and layout, the HTML parser, Wasm and more. The root
  `Cargo.toml` lists about 35 workspace crates (`libjs_rust`, `libweb_rust`,
  `libgfx_rust`, `libipc_rust`…). They are path dependencies of each other
  and of the C++ build; none is published.
- **Process model.** A UI process, a WebContent process per page and helpers:
  RequestServer, ImageDecoder, Compositor, AudioServer, VideoDecoderServer.
  The macOS app is 217 MB.
- **Headless modes.** `--headless[=screenshot|layout-tree|text|manual|terminal]`.
  - `screenshot` takes `--screenshot-path`, `--screenshot-delay`,
    `--window-width` and `--window-height`.
  - `terminal` takes over the terminal; its processes log to
    `/tmp/ladybird-terminal-PID.log`. It draws frames as Kitty graphics
    (`a=T`, then `a=f` for damaged regions), through temporary files with an
    inline fallback. It reads SGR-pixel mouse, the Kitty keyboard protocol,
    focus and bracketed paste, and follows `SIGWINCH`. The last row is its
    status line.
- **Embedding.** Only through its own C++ classes (`HeadlessWebView`,
  `TerminalWebView`), with no stable API. Public pull requests stopped on
  2026-06-05; only maintainers open them now.
- **Measured here.** It renders Axel's thread page the same as Chrome,
  JavaScript, cookies and the event stream included. Terminal mode
  through `axel web pane` uses about 285 MB in eight processes, idle, and
  exits with every helper when the pane hangs up.
- **What we take.** Its terminal mode, run as a program, as the web pane
  renderer (RFD 0007).
- **What we don't.** Linking it, its internal crates, and forking it.
