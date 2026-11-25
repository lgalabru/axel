# RFD 0004: Linked devices: one owner, many machines, one log

State: idea
Authors: Ludo  ·  Started: 2026-11-25

## Problem

Axel has one owner key per machine. A person with two machines therefore has two
identities, two agent registries and two lists of threads. Issue #40 describes this
from a Mac and a Linux box, both inside Rex:

- **No single list of threads.** Each home only sees its own threads. The P2 plan in
  `DESIGN.md` ("aggregate their DBs into one dashboard") reads databases side by side.
  It does not let you work a thread from either machine.
- **Agent names collide.** A host refuses an agent whose name it already holds under
  another key (`agent name … already exists`, `crates/store/src/local.rs:268`). So two
  homes that each have a `reviewer` can't use the same host.
- **The only way to be "you" on a second machine is to copy the owner secret.** That
  undoes "keys never leave home" (ADR 0004), and one stolen laptop then exposes the
  owner and every agent.
- **Nothing can be revoked.** Attestations carry conditions but there is no revocation.
  A lost machine's keys stay valid forever.
- **Losing a machine loses its agents.** Agent keys are random seeds in the keychain or
  `~/.axel/secrets.json`, with no recovery path.

Out of scope: scheduled work (#40 section 3, a docs page) and several humans on one
thread, which participants already handle.

## Sketch

### Three levels of key

```
owner O ──attests──▶ device D1 (this Mac)   ──attests──▶ agents of D1
        └─attests──▶ device D2 (Linux box)  ──attests──▶ agents of D2
```

- **Owner key O.** It's who you are. It's a thread participant, and it signs device
  links and revocations, nothing else day to day. It can live on one machine, or
  offline after phase 6.
- **Device key D.** Each machine has one, generated locally and never copied. O attests
  it once when the machine is linked. Everything a human does on that machine is signed
  by D, and authorization resolves D to O. It works like today's job delegation
  (`axel:delegate:v1`), but with no thread or time bound, ending only when O revokes it.
- **Agent key A.** The device attests it, not O. New agents may derive their key:
  `A = HKDF-SHA512(ikm = D_seed, info = "axel:agent:v1:" || agent_id)`. `agent_id` is a
  ULID, not the name, so renaming never re-keys. Ed25519 only allows hardened
  derivation, so only the holder of `D_seed` can derive. That is the point: one backed-up
  seed per machine restores its agents.

### Wire

- `kind 120 owner.device`: signed by O, content `{op: link|revoke, device, label}`, a tag
  `["d", device_pubkey]`. It isn't thread-scoped and travels to every device of O.
- Agents attested by a device carry a chain: `["auth", D, conditions, sig]` plus
  `["dev", O, D, sig]`. A replica that has the kind-120 link may accept the short form.
- `kind 121 agent.announce`: signed by the agent's device, content
  `{agent, name, harness, device}`. It lets other devices list and `@mention` agents
  they don't hold. A name is shown as `reviewer@linux` only when it clashes.

### Authorization

`authorize` (`crates/store/src/log.rs:425`) keeps its order: the signer is a
participant, or the signer's attested owner is. `attested_owner` walks one more link:
agent → device → owner, and device → owner for human envelopes. A revoked device
authorizes nothing created after the revocation. Envelopes that fail are stored with
`rejected_reason` as today, so a later link revives them.

### Sync, carried by Rex

This builds on RFD 0003, which makes Axel a native Rex client with blocks of its own.

Devices of one owner sync **every** thread, not only threads shared with a host, with
the existing per-peer `seq` cursors (`crates/store/src/remote.rs`). Rex does
everything around the bytes; Axel keeps the signatures, so Rex is trusted to deliver,
never to say what is true.

| Need | What Rex gives (checked 2026-11-25 against API revision 19) |
|---|---|
| Who my machines are | `hosts.list`: each host with a label and endpoints. `server.status` gives each server a stable `host_id`, which `environments.rex_host_id` already records. |
| Who is calling | A call through a host is authenticated by Rex's built-in Tailscale. `rex -S mini api call server.whoami` reports `kind: tailscale`, the login and the calling node. With `allow_tailnet_members: false`, only the owner's account gets in. |
| A process that outlives the laptop | `axel sync serve` runs in a Rex block on each host. Rex keeps it running with no client attached, so we need no launchd or systemd unit. |
| A byte channel | `rex forward`: TCP to the host's `localhost`, over Rex. `rex -S HOST run --wait` only returns an exit status, so it can't carry data. |
| A push instead of a poll | `rex -S HOST events -s SESSION --json` streams a session's events, including custom events from its blocks. The serve block can raise a program status when its `seq` moves, and a follower pulls on that event. |
| Pairing without codes | `axel device link HOST` runs on a machine holding O. It starts `axel device init` on the new machine with `rex -S HOST run --wait`, then reads its public key over a forwarded port, then signs the link. Your Rex host list is the proof that the machine is yours. |

The device link records the Rex `host_id` next to the device key, so `reviewer@linux`
and the Rex host `linux` are the same name.

The local-only tables stay local: keys, `environments`, Rex pane ids, read frontiers.
Conversation items keep flowing one way, from the device running the session.

What Rex doesn't give: storage, merging, or a key that can sign. Tailscale identity
is per account and per node, not an Ed25519 key, so it can vouch for a machine at
pairing time but can't replace device keys.

## Migration plan

Each phase ships on its own, adds and never rewrites, and leaves every existing
signature valid. A build that doesn't know a phase stores the new envelopes without
projecting them, and projects them after it upgrades. Schema changes go through the
upgrade safeguards of ADR 0011: a copy before migrating, fingerprints, refusal with a way
back.

**Phase 0: prerequisites.**
- Land the ADR 0011 store work.
- Key the agents a host runs by pubkey, not by name, so the #40 collision is fixed
  before anything else.
- Rollback: none needed.

**Phase 1: device keys on single machines.**
- Migration v13 adds `devices (pubkey, label, link_event_id, revoked_at)`.
- On first open, a build with O in its keystore generates D, signs a kind-120 link and
  stores D next to O.
- The identity crate verifies two-link chains; `attested_owner` resolves them.
- Nothing else changes: humans still sign with O, agents are still attested by O.
- Exit criterion: every replica verifies the chain, and replaying the log of a large
  database (the 300 MB one from 2026-11-22) projects identically to before.

**Phase 2: humans sign with D.**
- The CLI and TUI sign as D, and authorization resolves D to O.
- Old builds on other peers can't resolve D yet. They store those envelopes as rejected
  and revive them when they upgrade, so phase 1 must reach every peer first.
- Rollback: sign with O again. D's envelopes stay valid.

**Phase 3: new agents attested by D.**
- `axel agent new` attests with D, and may derive A as above.
- Existing agents keep their key and their O attestation. Re-keying would orphan their
  history, inbox and policy, which all hang off the pubkey.
- `axel agent rekey` is not part of this plan.

**Phase 4: link a second machine.**
- On a machine holding O, `axel device link HOST` drives the whole flow through Rex,
  as described under "Sync, carried by Rex". The new machine makes D2 itself, and O
  signs the kind-120 link. No pairing code is needed: the Rex host list is the trust
  root.
- The new machine never holds O.
- `axel device revoke NAME` signs the revocation.

**Phase 5: owner-wide sync over Rex.**
- Sync every thread between devices.
- Add kind-121 agent announcements and a `known_agents` table, so the dashboard lists
  threads and agents from every device.
- A device's own agents stay in `agents`, where names remain unique.

**Phase 6: merging two existing homes (the #40 case).**
- Pick one owner, O1. O1 signs a kind-120 link that adopts the other home's owner key
  O2 as a device. O2 already attests that home's agents, so the chain O1 → O2 → agents
  works with no re-signing.
- O2 stays an owner participant of its own threads.
- O1 is added as owner to each of them with a `thread.participant` envelope signed by
  O2, so they show as O1's threads.

**Phase 7: optional, cold owner.**
- O moves out of the daily keystore into a recovery file or a hardware key, needed only
  to link or revoke devices.
- A lost laptop then costs one device and its agents, not the identity.

## Open questions

- **Backdating a revoked device.** A compromised device can sign with an old
  `created_at`. Options: reject by local arrival order (replicas would disagree), or
  have every device sign a periodic checkpoint and refuse anything not covered by one.
  Needs a threat-model spike.
- **Does a program-status change reach a remote `rex events` stream?** The CLI docs
  say `rex events --session` includes custom events from blocks. Find out by raising a
  status in a block on `mini` while following `rex -S mini events -s SESSION --json`
  here. If it doesn't arrive, fall back to holding a pull open over the forwarded port.
- **Should sync live in an Axel block plugin?** RFD 0003 plans thread, inbox and
  dashboard blocks with their own methods and events, and Rex restores plugin blocks
  after a restart. A sync block would give pushes and survive restarts, with no
  terminal in between. That waits on Rex publishing the block plugin protocol. Until
  then, `axel sync serve` runs in a terminal block.
- **Derived or random keys for new agents?** Derivation needs a stable per-device seed
  and a recovery story. It's worth it only if phase 7 happens. Decide after phase 4.
- **How does `axel session move` fit?** It moves Claude sessions between home and hosts.
  Between two devices of one owner it might become an ordinary handoff. Revisit in
  phase 5.

## Risks

- **It changes ADR 0004's trust model.** A bug in chain verification authorizes the
  wrong signer everywhere. Mitigation: property tests over random chains, and failing
  closed as `attested_owner` does today.
- **Rollout order matters.** Phase 2 before phase 1 has reached every peer leaves
  envelopes rejected until those peers upgrade. They are revived on upgrade, not lost,
  but the gap is visible.
- **Full sync is more data.** One busy database reached 300 MB. Initial sync of a new
  device has to be resumable and paged, which the `seq` cursors already allow.
- **Two keystores to keep safe** during phases 1 to 6, until O goes cold.

## What we learned

- 2026-11-25: Rex already gives authenticated reachability between the owner's
  machines: built-in Tailscale, `hosts.list`, a stable `host_id`, `rex forward`, and
  cross-host `rex events`. It has no stdin/stdout pipe for remote commands. Sync,
  pairing and liveness lean on it. Signatures stay in Axel.
- 2026-11-25: Turso Sync, a local-first SQLite sync from the Turso team, was considered
  as the replication layer and rejected:
  - It needs a sync server.
  - It resolves conflicts by last push wins, where the log merges by union.
  - It writes rows without going through `Store::append`'s verification.
  - Its engine is beta.
  The signed log already does the hard part. See the conversation behind issue #40.
