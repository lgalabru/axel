# Axel v2 — Design

Axel is agent orchestration designed for Rex: threads and named agents, back in the terminal. It runs inside
[Rex](https://www.superlogical.com/rex) and treats Rex as the window manager:
Axel never draws windows, it asks Rex for panes.

This is a from-scratch reboot. The previous Axel (tmux launcher + macOS app) and two
other tools the author used (Buzz, t3code) were studied first; see `RESEARCH.md`.
Code is imported piece by piece, never by trimming the old repo. This file says what
Axel is; why it is that way is in `adr/` (one decision per file), and ideas that
aren't decisions yet are in `rfd/`.

## Principles

1. **Terminal first.** The UI is a TUI in a Rex block. Agents run as their real CLIs in
   Rex blocks, so you keep their native UX. Axel adds the layer around them.
2. **Structured over scraped.** Every signal comes through a protocol: Claude Code
   hooks, Codex notify, ACP frames, OSC 7501 program status. Never `capture-pane`
   and grep. (This is where Axel v1 failed.)
3. **Agents are principals.** An agent is a named identity with an Ed25519 key,
   attested by its owner, addressable as `@name`, with an inbox. Identity outlives
   any process or thread. (Borrowed from Buzz, re-keyed from secp256k1 to Ed25519.)
4. **Local-first, the signed log is the truth.** One `~/.axel/axel.db` per machine holds
   the event log and its projections. CLI, hooks and the TUI all append to it. Replicas
   merge by union. No daemon in v0; one appears only for always-on sync.
5. **Threads are the primitive, and they are multiplayer.** A thread is an intent plus
   the signed history of everything done in its name, by any participant's agents, on any
   environment. It survives processes, panes and machines. (Thread/run/session split
   from t3code; signed events from Buzz.)
6. **Capabilities, not names.** Harness behaviour is driven by a capability struct
   (hooks? acp? status-escapes?) so new harnesses slot in without `match harness`.

## Glossary

| Term | Meaning |
|---|---|
| **Environment** | A machine running Axel: its filesystem, credentials, DB. Identified by a stable `environment_id`, and by the Rex `host_id` when reachable through Rex. |
| **Project** | A directory on an environment, usually a git root, optionally with an `AXEL.md`. Projects on different environments that share a `repo_key` (canonical remote URL) are the same repository. |
| **Agent** | A named identity (Ed25519 keypair, owner attestation) plus a *profile*: harness, model, skills, default prompt, colour. Reusable across threads. Has an inbox. |
| **Harness** | The program that runs the agent: `claude`, `codex`, `opencode`, or a custom command. |
| **Thread** | **The primitive.** An intent (the initial prompt) and everything done in its name: attached to 0..n projects, realized by many agent *sessions* over time, on one or many environments. States: `open → done → archived`. Live state is derived from its sessions. |
| **Session** | One agent instance working on a thread: agent + environment + (optional) project + harness process. Owns the Rex block, the harness's own session id (for `--resume`), the transcript, status and token usage. States: `starting → running ⇄ blocked ⇄ idle → ended`. |
| **Message** | A signed envelope from one identity (you, or an agent) addressed to identities via `p` tags. Stored in the DB. Kinds: `note`, `task`, `permission_request`, `permission_reply`, `question`, `answer`, `status`, `handoff`. |
| **Inbox** | For an identity: messages that mention it, plus live status records from its sessions, minus what it has read (a per-identity read frontier). |
| **Owner** | You. One Ed25519 key per environment, generated on first run, kept in the OS keychain. Signs agent attestations and your own messages. |

## The thread model

The thread is the primitive. Getting this right matters more than any screen.

```
Thread            an intent; title; state open|done|archived; optional parent (fork|subagent|delegation)
 ├── ThreadProject*   0..n projects the thread touches, each with its workspace
 │                    (in place | worktree + branch). 0 projects = a scratch thread
 │                    (~/.axel/scratch/<slug>, promotable later, as in t3code).
 ├── Session*         agent instances over time: agent × environment × (project?) × harness process.
 │     ├── harness_session_id   Claude `session_id` / Codex `thread-id`, for --resume
 │     ├── transcript_path      where usage is read from
 │     ├── rex ids              the pane currently hosting it (if any)
 │     ├── Status               live: working | blocked(kind) | done | error | idle
 │     └── Usage                tokens + cost; thread totals are sums over sessions
 └── Message*         signed envelopes tagged ["t", thread_id] and optionally ["s", session_id]
```

Rules:
- **IDs are Axel's.** Harness ids (`claude --resume …`, Codex rollouts) are references held
  by a session, never identity (t3code's "app IDs primary, provider IDs are refs").
- **A thread never moves; sessions do.** "Move Atlas to studio" = end the local session,
  start a new session for the same thread on `studio`, resuming the harness conversation
  from the copied transcript and the pushed branch. The thread keeps its id, projects,
  messages and usage history. "Add Nova to this thread" = another session, same thread.
- **Projects are per environment; repositories are not.** A session on environment B for a
  thread attached to project P (environment A) resolves P's `repo_key` to a project on B,
  creating it if B has a checkout.
- **Thread state is coarse** (`open|done|archived`) and set by you or by policy. Live
  state (`working`, `blocked`, …) is derived from the thread's non-ended sessions, so a
  thread with two agents shows `blocked` if either is.
- **Hooks address a session.** Harness processes get `AXEL_THREAD`, `AXEL_SESSION`,
  `AXEL_AGENT` in their environment; status and usage rows are keyed by session.

Schema:

```sql
threads(id PK, title, intent, state, parent_thread_id, relationship, created_at, updated_at, done_at)
thread_projects(thread_id, project_id, workspace_kind, branch, worktree_path, added_at, PRIMARY KEY(thread_id, project_id))
sessions(id PK, thread_id, agent_pubkey, environment_id, project_id NULL, harness, runtime_mode,
         harness_session_id NULL, transcript_path NULL, resumed_from_session_id NULL,
         rex_session_id, rex_window_id, rex_block_id, state, started_at, ended_at, ended_reason)
status(session_id PK, state, kind, title, msg, progress, updated_at)
usage(session_id PK, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, turns, cost_usd_est, transcript_offset, updated_at)
usage_samples(session_id, at, input_tokens, output_tokens)
messages(id PK, from_pubkey, created_at, kind, tags_json, content, sig, thread_id NULL, session_id NULL)
projects(id PK, environment_id, path, name, repo_key NULL, has_axel_md, last_opened_at, UNIQUE(environment_id, path))
```

CLI surface that follows from the model:

```
axel thread new "intent" --agent atlas [--project P]... [--worktree [BRANCH]]   # thread + first session
axel thread spawn THREAD --agent nova [--env studio]                         # another session on the thread
axel thread resume THREAD [--env studio]                                     # new session continuing the last one (--resume)
axel thread attach THREAD --project P [--worktree BRANCH]
axel thread ls | show THREAD | done THREAD | archive THREAD
```

## Multiplayer: a thread is a signed event log

Several humans, each with their own agents, on several machines, work the same thread.
So a thread is not a row owned by one database. It is an **append-only log of signed
envelopes**, and every table above is a **projection** of that log that any replica can
rebuild. Two replicas merge by taking the union of envelopes (ids are content hashes, so
duplicates are free) and replaying. This is the t3code event-log design with Buzz's
signed-event lineage, minus the relay.

- **Thread id = the id of its root envelope** (`THREAD_CREATE`). Globally unique and
  self-certifying; no coordination needed to create threads offline.
- **Every change is an envelope** signed by the human (owner key) or agent that made it:

  | kind | name | content / tags |
  |---|---|---|
  | 100 | `thread.create` | content = intent; tags `title` |
  | 101 | `thread.update` | content = JSON `{title?, state?}` |
  | 102 | `thread.project` | content = JSON `{op: attach\|detach, repo_key, path_hint, workspace_kind, branch}` |
  | 103 | `thread.participant` | tags `["p", pubkey]`, content = JSON `{role: owner\|member\|agent, op: add\|remove}` |
  | 110 | `session.start` | tags `["s", session_id]` `["p", agent]`; content = JSON `{environment_id, environment_label, project_repo_key?, harness, runtime_mode, resumed_from?}` |
  | 111 | `session.update` | tags `s`; content = JSON `{state?, harness_session_id?, transcript_path?, rex: {session, window, block}?}` |
  | 112 | `session.end` | tags `s`; content = JSON `{reason}` |
  | 20 | `status` | tags `s`; content = JSON `{state, kind?, title?, msg?, progress?}` |
  | 21 | `usage` | tags `s`; content = JSON delta `{input, output, cache_read, cache_write, turns, cost_usd_est}` |
  | 1,2,10–13,30 | notes, tasks, requests, replies, handoff | as before, tags `t` and optionally `s` |

  All thread-scoped envelopes carry `["t", thread_id]`. Session ids are ULIDs minted by
  the starting replica (unique enough; the `s` tag binds them to the signer).
- **Authorization at ingest.** The root signer is the thread's first owner. A `thread.
  participant add` must be signed by an owner. Any envelope on the thread must be signed
  by a participant, or by an agent whose owner attestation points at a participant. Local
  `status`/`usage` from one's own agents is always accepted. Unauthorized envelopes are
  stored but not projected (`events.rejected_reason`), so a later participant-add can
  revive them.
- **Conflicts.** Projections apply events in `(created_at, id)` order; scalar fields are
  last-writer-wins, sets (projects, participants) are add/remove with LWW per element.
  Counters (`usage`) are sums of deltas, so they merge exactly.
- **Local-only state** (what is not an envelope): the agents registry with its keys,
  environment rows, Rex pane ids are still *recorded in events* for others to see where
  work runs, but they are only *actionable* on the replica that owns them.
- **Sync (P1).** `axel sync PEER` exchanges envelopes since a per-peer cursor over SSH or
  Tailscale (two Axel replicas, no server). A relay (P2) is just a third replica that is
  always on. Names like `@atlas` are resolved within a thread's participants and their
  agents; the canonical reference is always the pubkey.

The `axel-store` crate therefore holds: `events` (the log: every envelope, with
`applied_at` and `rejected_reason`), the projections (`threads`, `thread_projects`,
`thread_participants`, `sessions`, `status`, `usage`, `usage_samples`, `mentions`,
`message_refs`, `read_frontiers`), and local-only `agents`, `environments`, `projects`.
Writes go through typed methods that build, sign and append envelopes, then project them.

## The launch flow

```
$ axel                               # in any Rex terminal
```

1. Axel sees `REX_SESSION`/`REX_BLOCK`, and asks Rex for a **floating layer** in the
   upper part of the window (`session.new_layer`) split in two, like a thread's
   stream and composer: the **prompt** (`axel ui --screen launch`) on top, focused,
   and the **recent threads** (`axel ui --screen threads`) below. Outside Rex, the
   prompt runs inline on its own.
2. **Type and press Enter.** That launches at once with the defaults, shown under the
   prompt: the agent used last (or the one a leading `@name` names, which is dropped
   from the prompt), working in place in the current directory. The prompt becomes
   the thread title and the first prompt. With no agent yet, Enter asks who.
   - **↓** moves to the recent threads: open ones first, then the settled ones
     (dimmed, ✓). Type to filter, Enter opens one (docked, see 3), ↑ on the first row
     goes back to the prompt. Esc in either pane closes both.
   - In that pane, like t3code's sidebar: **Ctrl+S** settles a thread (finished,
     still listed) or reopens a settled one; **Ctrl+X** archives it (hidden; `axel
     thread reopen` brings it back); **Ctrl+D** twice deletes it. Archive and delete
     stop the thread's live sessions first. A deleted thread is a tombstone
     (`state = deleted`): the log is append-only, so it stays in the log but is hidden
     everywhere, including `axel thread ls --all`. The CLI has the same verbs:
     `axel thread done|reopen|archive|delete`.
   - **Tab** opens the full form instead, and the prompt pane grows to fit it
     (Tab/Shift-Tab to move, Enter to confirm):
   - **Where?** Project picker, multi-select (Space toggles): recent projects from the
     DB, fuzzy path entry, or none for a scratch thread.
   - **Who?** Agent picker: existing agents with harness, last-seen and unread count,
     or `+ new agent` → name, harness, model, skills. Creating one generates an
     Ed25519 key, stores it in the keychain, and writes an owner attestation.
   - **Workspace:** in place, or a new worktree (branch name suggested from the title).
3. On confirm, Axel:
   - creates the thread, attaches the projects, and creates the first session,
   - prepares the workspace (worktree if asked),
   - installs the agent's skills into the harness's expected location (sources from
     `AXEL.md`: directories, and skills.sh / GitHub skills pinned to a commit in
     `AXEL.lock`; a directory skill is linked whole, scripts and all), and links
     `AXEL.md` as `CLAUDE.md` / `AGENTS.md` (relative; when the project has its own,
     `AXEL.md` goes into the appended system prompt instead),
   - writes harness hook config so events flow back (see *Signals*),
   - starts the session: a managed one is hosted by `axel session host`; a pane one
     runs the harness with `AXEL_THREAD`, `AXEL_SESSION`, `AXEL_AGENT`, `AXEL_DB` in
     its environment,
   - **docks the thread view** in the window: the stream above, the composer below,
     in the place of the shell `axel` was typed in (`$AXEL_ORIGIN_BLOCK`). That shell
     is split, then taken out of the window with `session.detach_block`, still running;
     nothing else moves. When the composer closes, by Esc or by its pane's ×, it moves
     the shell back with `session.move_block` and the shell fills the space again.
     The × hangs the composer up about a second before it's killed, so a SIGHUP
     handler does that work from its own thread, its `rex` calls ignoring the hangup.
     A shell that's busy (its foreground program isn't the shell) keeps its place, and
     the thread docks as a column on the right instead,
   - closes the floating layer.
4. `axel` again toggles the floating pane. It opens on the **inbox** when anything is
   blocked or unread, otherwise on the launcher. A Rex `init.lua` action
   (`rex do axel`) can bind this to a key.

## Signals (how status gets back without scraping)

| Harness | Mechanism | Notes |
|---|---|---|
| Claude Code | `.claude/settings.json` hooks → `axel hook claude` | Hook reads the event JSON on stdin and `$AXEL_THREAD` from its *inherited environment*, so a workspace-wide settings file no longer misroutes events (the v1 bug). |
| Codex | `notify` in config → `axel hook codex` | Same shape. |
| Any ACP agent | `axel acp <target>` stdio relay (ported from v1) | Parses `session/request_permission`, `session/update` frames. |
| Anything else | `axel status working|blocked|done "msg"` | Plain CLI. |

Every hook invocation does two things: appends a `status` or `permission_request`
message to the DB, and writes **OSC 7501** to `/dev/tty` so Rex itself shows the
blocked/working indicator in tab headers and the session picker. Axel's inbox and Rex's
native indicators therefore always agree.

A **managed** session's host does the same into its own pane, on every change, following
the Program Status Protocol (revision 0.4): `working` with the head of the prompt when a
turn starts or resumes after an answer, `blocked` + `kind=permission` with the tool's
title while it waits for you, then `done` with the head of the reply when the turn ends
(`idle` if it was cancelled, `error` if it failed), and `clear` when the session ends.
Each report replaces the record whole, so every one carries `app=axel` and the agent's
name as `title`.

Replies (approve/deny, answer a question) are delivered through the protocol that asked:
ACP gets a JSON-RPC response; Claude Code hooks get a hook response on stdout
(`PermissionRequest` hooks can allow/deny); a harness with neither gets the reply
appended to its inbox for the agent to read on its next turn. No `send-keys`.

## Autopilot: no babysitting

t3code achieves "only ask me about design decisions" by running Claude through the
Agent SDK with `permissionMode: bypassPermissions` (its `full-access` runtime mode) and
surfacing `AskUserQuestion` as a user-input request. Axel does the same with the CLI.
Each agent (overridable per thread) has a **runtime mode**:

| Mode | Claude Code flags | Codex | Meaning |
|---|---|---|---|
| `autopilot` (default) | `--permission-mode bypassPermissions` | `--full-auto` / `-a never -s workspace-write` | Never asks for tool permission. A `PreToolUse` guard hook still denies a short list of irreversible actions. |
| `edits` | `--permission-mode acceptEdits` | `-a on-request` | File edits auto-approved; shell commands prompt. |
| `supervised` | default | default | Every prompt is forwarded to the inbox. |
| `plan` | `--permission-mode plan` | read-only sandbox | Reads only; produces a plan. |

The **guard** (`axel hook claude` on `PreToolUse`) returns
`permissionDecision: "deny"` with a reason for: `git push --force`, `git reset --hard`,
`rm -rf` outside the workspace, `git clean -f`, pushing to protected branches, and
anything in the agent's `disallowed_tools`. Everything else passes. Runtime mode is
separate from **interaction mode** (`build` | `plan`), as in t3code.

**Design decisions** reach you because the agent uses `AskUserQuestion` (Claude) or
asks in prose then stops (Codex). The guard sees `AskUserQuestion` in `PreToolUse`,
marks the thread `blocked kind=question`, and posts a `question` message into the
owner's inbox with the options. You answer in the agent's pane (native UI) or, in P1,
from the Axel inbox through a `PermissionRequest`-style hook reply.

## Token accounting

No telemetry server. Harnesses already write structured transcripts:

- Claude Code: `transcript_path` arrives in every hook payload. Each assistant message
  carries `usage{input_tokens, output_tokens, cache_creation_input_tokens,
  cache_read_input_tokens}`. `axel hook claude` tails the file from the last seen byte
  offset on `PostToolUse`/`Stop` and adds the deltas to the thread's `usage` row.
- Codex: `~/.codex/sessions/**/rollout-*.jsonl` has `token_count` events with
  `total_token_usage{input_tokens, cached_input_tokens, output_tokens, ...}`.
  `axel hook codex` (notify) locates the rollout by session id and records totals.
- Cost is estimated from a small per-model price table (`axel-config::pricing`), shown
  as an estimate.

## Dashboard (the main screen)

`axel` opens on the **dashboard**, styled after bashtop/cxgpu: dense, boxed widgets,
one accent palette, braille sparklines, keyboard-driven.

```
┌ agents ──────────────────────────────────────┐┌ environments ─────────────┐
│ ● Atlas    claude  axel-next   local  12.4k ▂▃▅│ ● macbook    3 agents 41k │
│ ◐ Nova     codex   pay-kit     studio  8.1k ▁▂▂│ ○ studio     1 agent  8k  │
│ ✓ Rex      claude  axel        local   2.0k    │ ○ cloud-1    offline      │
└──────────────────────────────────────────────┘└───────────────────────────┘
┌ tokens (1h) ────────────────────────────────┐┌ inbox ─────────────────────┐
│ ▁▂▃▅▇▆▅▃▂▁▂▃▅▇ in 310k  out 42k  $1.84 est  ││ ? Atlas: pick ORM: sqlx/.. │
│                                             ││ ✓ Nova: turn finished      │
└─────────────────────────────────────────────┘└────────────────────────────┘
 n new thread   enter open pane   i inbox   e environments   q quit
```

Widgets: **threads** (live glyph, title, agents on it, projects, environments, sessions
count, tokens, sparkline, age), **agents** (state glyph, name, harness, current thread,
environment, tokens), **tokens** (1h series, totals, estimated cost), **inbox**
(questions and permission requests first, then recent). `Enter` on a thread or agent
focuses the Rex window of its latest live session; `n` opens the launcher; `s` spawns
another agent onto the selected thread; `x` ends a session.

## Remote hosts

A **host** is another machine with Axel that runs agents for this one (the Mac mini at
home, say), so you can ask, close the laptop, and find the results when you're back.
`axel host add mini [--install]` copies this binary over SSH, installs what's missing
(Node, Claude Code, the ACP adapter), and records the host with its **host key** (its
Axel owner key). Claude Code must be logged in once on the host, from a pane of its own
Rex (`rex -S mini run -- claude`): the keychain isn't reachable from SSH.

**Asking.** `@atlas@mini …` in the composer, `axel send`, or the MCP `send` tool
(`to: ["atlas@mini"]`) summons atlas on the host. An agent can have a **default host**
(`axel agent edit atlas --host mini`, local-only like its policy): then plain `@atlas`,
the launcher, `axel thread new` and `axel thread spawn` run it there, and `@atlas@local`
or `--host local` runs it here. Attachments travel with the job or the relayed prompt
(base64, 32 MB per prompt). Everything goes over SSH (one
ControlMaster connection), each call being `axel host rpc` with one JSON request:

1. The thread's envelopes are pushed, and the thread is shared with the host from then on.
2. The code: a snapshot commit of the working tree (tracked + untracked, not ignored;
   index and branches untouched) is pushed into `~/.axel/repos/<repo>.git` on the host.
3. The host makes a **job key** (kept in its keystore) and atlas signs a **delegation**
   to it for this thread, valid 7 days. The job (agent profile + attestation, its reach
   and budget, the agents it may reach so `@name` resolves there, delegation, prompt,
   branch, attachments) is started: a worktree
   on `axel/<thread>-<agent>` and a managed session in the host's `axel` Rex session,
   on autopilot (nobody can answer prompts; the guard hook still runs). Rex shows
   OSC 9 notifications when a turn ends or a permission is needed.
4. A background follower (`axel sync mini --follow`) pulls envelopes and conversation
   items every couple of seconds while the host has live sessions, so the thread view
   streams; git branches every 30 s and on state changes. It backs off while the laptop
   sleeps and resumes after; `axel ui` restarts followers after a reboot.

Composer prompts, steering, approvals and agent messages to a remote session are relayed
to its socket on the host. `axel thread stop` stops remote sessions too.

**Agents on a host messaging home.** On the host, `send` reaches the sessions it runs;
for anyone else it reports `awaiting_sync`. When the laptop pulls that message, it
delivers it as the sender would have from here: the sender's reach is checked, and a
summon is checked as a child of the sender's session (depth, loops, budget). Recipients
already running on the sending host are skipped, since the host delivered to them.

**Moving a session** (`axel session move SESSION --to mini|local`, Claude only): the
session stops and ends, and its Claude conversation file (`~/.claude/projects/<cwd>/
<id>.jsonl`) travels with the code; a new session on the same thread `session/load`s
the conversation in the new directory and gets a short "you were moved" prompt. Out:
the snapshot is taken from where the conversation last ran (its `cwd`), and the host's
branch is reset to it; a worktree Axel made here is cleaned after (kept under
`refs/axel/sent/<branch>`). Home: the branch and uncommitted work are fetched, checked
out in a worktree under `~/.axel/worktrees` with the host's uncommitted work left
uncommitted, and the session resumes there. While that branch is checked out here, later
fetches land beside it in `refs/axel/<host>/heads/<branch>` and are fast-forwarded in
on the next move home.

**Delegation** (`axel-identity::delegate`): `["deleg", agent, thread, conditions, sig]`,
`sig = Ed25519(agent_sk, "axel:delegate:v1:" || job_pk || ":" || thread || ":" ||
conditions)`, conditions always bounding time. One key per job means the signing key
alone says which agent an envelope is for, even with several agents of one thread on a
host. The host signs with the job key and
attaches the delegation and the agent's owner attestation. At ingest every replica
verifies them; a valid one makes the agent the envelope's **author**
(`axel_store::author`), which is what authorization and projections use. A delegation
can't create, update or re-membership a thread, can't leave its thread, and expires.
Agent keys never leave home; forgetting a host (`axel host rm`) stops future jobs, and
outstanding delegations die on their own.

**Replication.** Envelopes move by per-peer `seq` cursors in both directions (only shared
threads go out). Conversation items are local rows, mutable while a turn streams; they
carry a revision kept by triggers (with tombstones for deletes) and flow one way, from
the host that runs the session. Results come back as the job branch plus
`refs/axel/<host>/<branch>` for uncommitted work.

Not yet: moving a session straight from one host to another (bring it home first),
moving Codex sessions, and agents on two different hosts messaging each other directly
(it goes through the laptop's sync).

## Identity and crypto

- **Curve:** Ed25519 via `ed25519-dalek` 3. Public keys are 32 bytes, shown as
  base58 (the same encoding as Solana addresses, so an agent key *is* a valid Solana
  address if you ever want it to be). Signatures are 64 bytes, verified with
  `verify_strict`.
- **Owner key:** one per environment, generated on first run.
- **Agent key:** generated on `axel agent new`.
- **Attestation** (adapted from Buzz NIP-OA):
  `preimage = "axel:agent-auth:v1:" || agent_pubkey_b58 || ":" || conditions`,
  `sig = Ed25519(owner_sk, preimage)`. Stored as `["auth", owner_pk, conditions, sig]`.
  `owner == agent` is rejected. Conditions mini-language (`kind=N`, `created_at<T`,
  `&`-joined) is ported verbatim.
- **Key storage:** OS keychain (`keyring` crate), service `axel`, one JSON blob under
  user `secrets` so there is one prompt per process; entries `owner` and
  `agent:<pubkey>`. Fallback: `~/.axel/secrets.json` written atomically with `0600`.
  Write-then-read-back verify. Refuse to launch an agent whose key is missing.
- **Encryption** is out of scope for v0 (local DB, single user). When needed, each
  identity gets a separate X25519 key rather than converting the Ed25519 key.

## Message envelope

NIP-01 shape, Ed25519 signature:

```
Envelope { id: [u8;32], from: PubKey, created_at: u64, kind: u32,
           tags: Vec<Vec<String>>, content: String, sig: [u8;64] }
id  = sha256(canonical_json([0, from_b58, created_at, kind, tags, content]))
sig = Ed25519(from_sk, id)
```

Tags: `["p", pubkey]` recipients (mention resolution from `@name`, ported from Buzz),
`["e", id, "", "root"|"reply"]` threading (NIP-10 port), `["t", thread_id]` the Axel
thread this concerns, `["h", harness]`, `["k", permission|question|auth]` for blocked
kinds.

## Data model (SQLite, WAL)

See *The thread model* above for `threads`, `thread_projects`, `sessions`, `status`,
`usage`, `usage_samples`, `messages`, `projects`. Plus:

```sql
environments(id PK, label, rex_host_id, is_local, created_at)
agents(pubkey PK, name UNIQUE, harness, model, skills_json, prompt, color,
       runtime_mode, auth_tag_json, created_at, last_seen_at, archived_at)
mentions(message_id, pubkey, PRIMARY KEY(message_id, pubkey))
message_refs(message_id, ref_id)
read_frontiers(pubkey, scope, through_created_at, through_id, PRIMARY KEY(pubkey, scope))
```

## Workspace layout

```
axel-next/
  Cargo.toml              workspace, edition 2024
  AXEL.md                 dogfood config for this repo (CLAUDE.md links to it)
  docs/DESIGN.md          this file
  docs/RESEARCH.md        what we learned from Rex, t3code, Buzz, Axel v1, Entire
  docs/IMPORTS.md         provenance of every imported module
  docs/adr/               decision records: why a design is what it is
  docs/rfd/               requests for discussion: ideas, risky research, open questions
  crates/
    axel-identity         Ed25519 keys, base58 encoding, attestation, keystore
    axel-inbox            envelope, kinds, threading, mentions, inbox queries
    axel-config           AXEL.md frontmatter+body, agent profiles, skills, harness drivers
    axel-store            SQLite schema, migrations, repositories
    axel-rex              Rex client (floating layers, windows, labels, status, events)
    axel-tui              ratatui launcher + inbox
    axel                  the `axel` binary
```

Imports: see `IMPORTS.md`. Nothing is copied with its relay/tmux/Tauri coupling.

## Phases

- **P0 (now):** skeleton; identity; inbox core; config+skills port; Rex client via
  CLI JSON; dashboard + launcher TUI; thread launch into a Rex window in autopilot;
  Claude hooks → status, questions, token usage; guard hook.
- **P1:** ACP runtime mode (chat rendered in the TUI, permissions answered in-protocol);
  worktree isolation port; `init.lua` action and key mode; Codex notify.
- **P2:** remote environments: register SSH/Rex hosts, launch on them, aggregate their
  DBs into one dashboard; `axel thread move` (transcript + branch handoff with
  `claude --resume`); t3code-style delegation (child thread elsewhere, follower here).
- **P3:** provider handoff mid-thread (context transfer records), checkpoints as hidden
  git refs.

## Non-goals

- A GUI. Rex is the GUI.
- Nostr interop. Ed25519 and a local DB are deliberate.
- Driving agents by typing into their terminals.

## Managed runtime (ACP) and the thread view

The `pane` runtime (the native CLI in a Rex pane) stays. The **managed** runtime is what
makes the thread the place you work: Axel drives the harness over **ACP** (Agent Client
Protocol, JSON-RPC over stdio) and owns the conversation. One protocol covers Claude,
Codex, OpenCode (`opencode acp`), Goose (`goose acp`) and others.

**Claude's adapter is built in** (`axel claude-acp`, `axel-acp/src/claude/`): a Rust port of
Zed's `claude-code-acp` 0.16.2 that drives the `claude` CLI directly, the way Anthropic's
Agent SDK does (`--output-format stream-json --input-format stream-json
--permission-prompt-tool stdio`, plus the SDK's control protocol: `initialize` with the
appended system prompt and a PostToolUse hook, `can_use_tool`, `interrupt`,
`set_permission_mode`, `set_model`). Up to the host it sends the same ACP as the Node
adapter: message and thought chunks, tool calls with the same titles, kinds, diffs and
locations, plans, slash commands, the same permission options and stop reasons, the same
replay on `session/load`. One difference on purpose: Claude uses its native Read / Write /
Edit / Bash instead of the adapter's `mcp__acp__*` stand-ins (Axel's `fs/*` handlers read
and write the disk just like them), which also saves Claude a ToolSearch round trip per
turn. No Node or npm is needed; `AXEL_ACP_CLAUDE=claude-code-acp` goes back to Zed's.
`crates/axel-acp/tests/claude_acp.rs` checks it against a fake `claude`.

**So is Codex's** (`axel codex-acp`, `axel-acp/src/codex/`). Zed's `codex-acp` compiles
OpenAI's engine in (codex-core, pinned to one Codex release), so it lags the `codex` you
have installed: it can't use models newer than its pin. Axel's drives your `codex` over
its own JSON-RPC protocol, `codex app-server` (`thread/start` or `thread/resume`,
`turn/start` with the session's approval policy, sandbox and model, `turn/interrupt`,
`model/list`, `review/start`, `thread/compact/start`), and answers Codex's command,
file-change and permission approvals from the host. Up to the host it sends what `codex-acp`
0.16.0 sends: chunks, tool calls with its titles, kinds, locations and diffs, plans, usage,
its slash commands and permission options, replay on `session/load`. On purpose, it differs
in three ways:

- it takes the host's mode ids, each an approval policy and a sandbox:
  - `default` asks unless the action is known to be safe, in a workspace-write sandbox;
  - `acceptEdits` asks on request, in a workspace-write sandbox;
  - `plan` is read-only;
  - `bypassPermissions` never asks and has no sandbox;
- it supports `session/set_model`;
- it passes the identity prompt as Codex developer instructions.

`AXEL_ACP_CODEX=codex-acp` goes back to Zed's. `crates/axel-acp/tests/codex_acp.rs` checks it
against a fake app-server.

**Custom ACP agents** use the same host as the built-in adapters. When a model
selector is advertised in `configOptions`, the host selects it with
`session/set_config_option`; otherwise it uses legacy `session/set_model`.
Grouped provider choices are supported. Standard `usage_update.used` and `size`
are context gauges. A cumulative USD `cost` is added only as it grows, without
counting it again from the adapter's usage extension or prompt reply. Other
currencies stay out of the USD total.

An agent without the live-steering capability queues prompts until its turn
finishes. Ctrl+S cancels that turn and sends the new prompt. The live-steering
extension is available to any ACP agent, not only Claude and Codex.

**A message sent while the agent works goes into its turn.** The agent reads it at its next
step instead of after the whole turn, and nothing is interrupted. ACP has no word for it, so
both adapters take two requests of Axel's own, and advertise them as
`agentCapabilities._meta.axel.steer`:

- `_axel/steer {sessionId, messageId, prompt}`: into the running turn; `accepted: false`
  when none is running (the host then sends it as a prompt). When it lands, the adapter
  sends `_axel/steered {messageId}`.
- `_axel/unsteer {sessionId, messageId}`: withdraw it if it hasn't landed;
  `withdrawn: false` once it has. The host edits a message by withdrawing it and sending the
  new text.

Claude takes such a message into its queue and folds it in at the turn's next step, between
tool calls. It echoes it back as it does, with `--replay-user-messages`, and one sent after
the turn's last step runs as a turn of its own, which the same prompt waits for.
`cancel_async_message` withdraws it. On `session/cancel`, the adapter withdraws what hasn't
landed and says so (`_axel/unsteered`), and the host runs those next. Codex takes one with
`turn/steer` against the active turn id, at once, so there is nothing to withdraw.

Messages from other agents still wait for their own turn, so the answer goes back to the
sender by itself. So does anything sent to an adapter without the capability. `Ctrl+S` still
interrupts the turn and starts a new one with the message.

```
thread view (TUI, any pane) ──unix socket──▶ axel session host <id> ──stdio/ACP──▶ adapter ──▶ harness
        ▲                                        │  prints the raw log (this pane *is* `l`)
        └──────── polls conversation_items ◀─────┘  appends every frame to ~/.axel/logs/<id>.jsonl
```

- **Session host**: `axel session host SESSION` runs in its own Rex block, labelled
  `agent · title`. It spawns the adapter, does `initialize` → `session/new` (cwd = the
  thread's workspace), then serves commands on `$AXEL_HOME/run/SESSION.sock` (JSON lines:
  `prompt`, `cancel`, `answer`, `status`, `stop`). It turns `session/update` notifications
  into `conversation_items` rows and answers `session/request_permission` by **policy**:
  `autopilot` allows everything the guard doesn't deny; `edits` allows read/edit tools and
  asks for execute; `supervised` asks for all; `plan` denies writes. "Asks" = a `Permission`
  item with `Pending` status; the thread view answers it through the socket. Every frame
  is logged to `~/.axel/logs/SESSION.jsonl`, and the host pane prints a readable, coloured
  line per event. That pane is the raw altitude.
- **Thread view**: the timeline at *conversation* altitude (your messages, agent prose,
  questions inline), tool bursts folded into one line; Space expands a fold into the
  *tools* altitude; `l` focuses the host pane (raw); Esc back. A composer at the bottom:
  text → `prompt`; `@name` → summon that agent onto the thread (new managed session) or
  hand the text to it if live; pick an option on a pending permission/question → `answer`.
  The text goes as typed, mention included. Without a mention it goes to the live session
  whose agent replied last; the footer shows the target (`→ nova`, `→ +vega` to summon).
- **Catch-up**: agents can't see each other's turns, so the host starts each turn with
  what its agent missed since its last one (`axel-acp/src/catchup.rs`): other sessions'
  assistant replies, the owner's and other agents' messages to them, `fyi` notes left in
  this session, and the live peers with their status. On the wire only; the stored user
  message stays as written. The first turn of a session gets the recent thread instead.
- **Questions**: Claude's `AskUserQuestion` is on. The Claude adapter asks each question
  as a `session/request_permission` whose options are the choices (ids `answer:<n>`, plus
  `answer:none` for "none of these"); the host never lets policy answer those, records a
  `Question` item, and reports `blocked` + `kind=question`. The composer opens each new
  question in a floating, centered pane (`axel thread answer THREAD ITEM`): ↑↓ and Enter,
  or a digit, answers it; Esc leaves it to the composer. The labels picked go back to
  Claude as the tool's `answers`. A multi-select question takes one pick.
- **Tool call details**: in the conversation pane each run of tool calls is an OSC 8
  link, `axel-links://tools?thread=…&fold=<first call id>&session=…&server=…`
  (`axel://` is the app's sign-in callback). A small handler app,
  `~/.axel/apps/Axel Links.app` (an AppleScript applet that declares the scheme;
  `axel links install`, or built on first use), hands each URL to `axel open-url`,
  which opens a floating Rex pane down the right of the window (titled with the agent and
  the number of calls) running `axel thread tools`: the run's agent and time span, then
  each call with its start, duration and status, its input as its kind reads best (a
  command and what it's for, an edit as a diff, a file and its lines, a search), and its
  output (as stored, up to 4 KB), printed as plain output so it scrolls natively; Esc or
  q closes it. Code is highlighted with syntect's bundled syntaxes (no themes: syntax
  scopes map to the terminal's own palette, like bat's `ansi` theme): commands as shell,
  edits and writes and what a read returned as their file's language.
- **Recovering a session**: a message for a managed session whose host has died
  (its socket refuses, or is gone) ends that session and resumes the agent on the
  thread, in its harness conversation (`--resume`/`session/load`), with the message as
  the first prompt. Any summon of an agent back onto a thread it worked on resumes its
  last conversation there. The reconciler counts every Rex block (`session.list_blocks`),
  detached ones included, so a minimized agent pane is never taken for a closed one.
- **Answers come back**: a turn another agent's message started (`Sender`, not `reply`)
  ends by returning its last assistant message to that agent: the host runs `axel send
  --reply --to <agent>` with the session's environment (signed, policy-checked and
  delivered as any message; tagged `reply`; never summons). The asker reads it framed as
  an answer the owner has already seen. A turn a reply started returns nothing, so
  agents can't ping-pong.
- **Asking for reach**: when `send` is refused only because the sender may not reach a
  recipient, Axel's MCP server asks the owner by MCP elicitation (one choice: allow this
  message, always allow, don't allow). The Claude adapter answers Claude's `elicitation`
  control request like a question, so it floats in front of the owner. "Always" writes
  the policy as `agent edit --allow` would; "this message" is a grant scoped to that one
  `send` call (a guard), honoured by both the message and summon checks. A client that
  can't elicit gets the refusal with the command, as before.
- **The window's name is the thread's**: a docked thread names its Rex window after its
  title, and the stream pane watches `rex events` for `window_label_changed`: a name typed
  over the window in Rex becomes the thread's title (Rex's placeholder and the title as
  Axel shortens it for the tab don't count, so Axel's own labelling renames nothing).
- **Composer panels**: Ctrl+D opens the thread controls. `t` selects a live agent
  to stop on this thread; `x` stops all live sessions. Stopping one agent leaves
  its configuration, history, other threads, and other agents intact.
  Ctrl+T opens a focused
  floating shell in the thread’s project directory, using its worktree when present.
  A thread without a local project uses the composer’s directory.
- **Terminal colours**: normal text and surfaces use the terminal's own foreground
  and background. Selections reverse those colours, and code explicitly resets
  its background and uses the terminal's blue. Approval cards do not force a dark
  surface. The same rendering works with light and dark Rex themes.
- **Composer arrows**: Up and Down move through the wrapped rows on screen,
  keeping the intended column across shorter lines. This also applies to recalled
  prompts. History changes only beyond the first or last screen row.
- **Typing in the message pane**: plain typing or pasted text goes into the composer
  directly below that stream, which takes focus. Existing draft text is preserved.
  The transfer is a paste, so digits cannot answer a waiting approval and newlines
  cannot send a message. Scrolling and selection stay native to the terminal.
- **Agent panes**: a managed session's host pane (`<agent> · <thread> · log`) starts
  minimized: a detached Rex block, running but in no window. ^L in the composer shows the
  log of the agent you're talking to beside the conversation (`session.move_block`), and
  minimizes it again (`session.detach_block`). The thread's stream pane reports one
  OSC 7501 record per live agent (`id=<agent>`, its state, `kind` when blocked), and a
  root record (no `id`) for the thread: the most urgent agent state, the thread's title,
  and each agent's state as `msg`. The root record is what Rex's tab and sidebar show.
  A finished turn is recorded `done` (cancelled: `idle`, failed: `error`), which Rex marks
  as an unseen result, and the pane rings the bell (BEL) whenever an agent becomes `done`
  or `blocked`, as Claude Code does when it finishes or needs you.
- **Notes**: `send` with `fyi` (`axel send --fyi`) tags the signed message `fyi`. To a
  live managed session it's `Command::Note`: a `UserMessage` item with `fyi: true`, no
  turn started, read in the next catch-up. It never summons. Inbox entries that already
  reached the recipient as a prompt or note are `handled`.
- **What is shared**: per completed turn, one signed `turn` envelope (kind 40): user text,
  assistant text head, tool-call counts, duration, stop reason, usage if known. Remote
  participants see the conversation; raw logs and full items stay on the environment.
- **Usage** for managed sessions: taken from adapter `_meta`/usage fields when present,
  else from Claude's transcript as for pane sessions. Best effort in P0.

## Turn checkpoints: what a turn changed

Autopilot only works if what comes after is cheap: what changed, is it right, and if
not, make it go away. So every turn in a git workspace leaves a **checkpoint**: a hidden
commit of the tree before the turn and another after it, made the way the remote handoff
snapshots a tree (`axel_acp::checkpoint`: a temporary index, tracked and untracked files,
not ignored ones, unsigned, on top of HEAD; a clean tree is HEAD itself). They live under
`refs/axel/checkpoints/<thread>/<session>/<ordinal>/{before,after}`, which nothing lists
or pushes. A snapshot has a time budget (10 s in the host, 6 s in a hook): past it, or
outside a repository, the turn simply has no checkpoint.

- **Managed sessions**: the host snapshots before the prompt goes out and after the turn
  ends, and notes the files the edit tool calls name (their ACP locations).
- **Pane sessions** (Claude): `UserPromptSubmit` opens the turn and takes `before`,
  `PostToolUse` notes `Write`/`Edit` paths, `Stop` takes `after` and shares the turn. Each
  hook is a fresh process, so the open turn sits in the local `turn_marks` table. (Codex
  has only `notify`, at the end of a turn: not covered yet.)
- **What is shared**: the `turn` envelope (kind 40) gains `changes`: the two commit ids,
  the file list (status, lines added and removed, `authored` when this session's edit
  tools touched the file) and `between_turns` when the tree at the start didn't match the
  previous turn's end. The diff itself stays in the repository that ran the turn.
- **Seeing it**: a dim change line under the turn in the thread view, `↗ 3 files  +42 −7`,
  or `2 files (+1 not this agent)` when other hands changed files during the turn. In Rex
  it's a link that opens the turn's diff in a floating pane; `axel thread diff THREAD
  [--session S] [--turn N] [--stat]` prints the same.
- **The diff pane** (`axel_tui::diff`): the default reader prints all changed
  files as ordinary terminal output, like the conversation and tool-call panes.
  Rex owns scrolling, trackpad momentum, text selection, and search. The reader
  does not use the alternate screen or capture the mouse. Output changes only
  when the terminal width settles after a resize, a layout is selected, or the
  turn or saved notes change. `s` switches split and unified layouts. `[` and
  `]` step between the session's turns. The agent's own files come first.
  Old and new text sit side by side when the code area has 110 columns.
  Syntax comes from the immutable before/after checkpoint blobs, parsed as
  complete files to preserve multiline state. Highlighting is bounded and
  optional. Terminal foreground and background queries derive soft addition
  and removal surfaces. Changed words use a slightly stronger tint while
  retaining syntax colours. Unsupported terminals use native surfaces and word
  underlines. Tabs and wide characters use terminal columns.
  `r` enters explicit line review on the alternate screen. Wide panes show a
  directory-grouped file navigator and a pinned file header. Left/Right pan
  long lines; clicking a file or line selects it. `n`/`p` jump between hunks.
  `v` marks lines, `c` writes a note, and Tab chooses the old or new side.
  `q` or Esc returns to native reading. With no edits, the printed diff stays
  intact. Notes wrap on their source side and share geometry with code.
  Drafts are kept in `~/.axel/reviews/`; Enter sends them as one message, a
  fenced `review` block per note with its path, line range, side, source text,
  and note. Delivery uses the live managed session, else summons the agent.
  Enter, undo, restore, and turn navigation work in both modes. See
  [ADR 0010](adr/0010-native-scrollback-for-diff-reading.md).
- **Undo** (`u` in the pane, `axel thread undo`): the reverse of the turn's
  `before..after` patch, limited to its authored files (all of them when none is
  marked), applied to the working tree only, after a snapshot of the tree under
  `refs/axel/checkpoints/<thread>/undo/<n>` so an undo can be undone. A hunk that later
  work overlaps stops it before anything is written; `R` (`--restore`) then rewrites the
  turn's files to their `before` content instead. Not offered while the agent is
  mid-turn. The agent gets a note saying which files went back, so it doesn't build on
  code that's gone.
- **Across environments**: a turn that ran on a host is read here after `git fetch` of
  `refs/axel/checkpoints/<thread>/*` from the host's copy of the repository, over the
  same SSH connection the handoff uses. Undo runs where the checkout is.
- **Pruning** (`axel gc`, also after `axel thread archive`): checkpoints of archived or
  deleted threads go after 7 days; on live threads, after 30 days only each session's
  latest 20 turns stay. Git's own gc reclaims the objects.
- **Not yet**: Codex pane sessions (no turn-start hook), and settings for the retention
  numbers. Why turns and not commits, and what to borrow from Entire's commit-time
  design: `adr/0001-turn-checkpoints-not-commit-checkpoints.md`.

