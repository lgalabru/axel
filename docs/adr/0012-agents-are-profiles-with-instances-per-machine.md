# ADR 0012: An agent is a profile with an instance per machine, and a machine's key is its device key

Status: accepted (2026-11-25). Supersedes ADR 0004. Explains `DESIGN.md` "Principles" 3,
"Linked devices" and "Identity and crypto".

## Context

ADR 0004 made an agent one named Ed25519 identity, attested by the owner of the machine
that made it. Issue #40 runs two homes, a Mac and a Linux box, each with its own owner
key. Each has a `reviewer`, and a host refused the second one: `agents.name` was unique.
Copying the owner secret to the second machine would undo "keys never leave home".
RFD 0004 sketched device keys under an owner key, and the owner asked that an agent be an
instance of a profile on a given machine.

## Decision

- **An agent is a profile and its instances.** The profile is what the agent is: name,
  harness, model, effort, skills, prompt, colour, mode. It belongs to the owner, has no
  key, and travels to every device as a kind-122 envelope. The newest one wins. An
  instance is the profile on one machine. It has an Ed25519 key attested by that
  machine, it signs, and it has the inbox. Names are unique per owner, not globally.
- **A machine's own key is its device key.** We don't mint a separate device key. The
  key every machine already has, called "owner key" in ADR 0004, plays both parts:
  - On a machine nobody linked, it is an owner.
  - Once another owner links it with a kind-120 envelope, it speaks for that owner.
- **Authorization compares principals.** A signer resolves through device links to the
  owner it speaks for, at most four links. A home linked as a device therefore brings
  its threads and agents with no re-signing.
- **Pairing proves the far end.** `axel device link` accepts an SSH host key or a code
  the owner compares on both machines. It does not depend on Tailscale.

## Consequences

- The #40 name clash is gone. Two owners may each have a `reviewer`. One owner's two
  machines run one `reviewer` as two instances.
- Linking two homes that each made a `reviewer` leaves one owner with two profiles of
  one name. Identical ones merge into the oldest on link; the owner settles the others
  with `axel device merge` (RFD 0004 phase 7). Until then, name lookups refuse to pick.
- Migration 13 turns each agent into a profile (`p-<pubkey>`) plus its one instance,
  keeping the key and attestation. `agents` is rebuilt to drop `name UNIQUE`.
- Any machine of an owner can edit a profile. Concurrent edits made apart resolve last
  write wins; the overwritten version stays in the log.
- The owner key stays on the machine that has it. RFD 0004's phase 8 (an offline owner)
  would need a separate device key on that machine and is not done.
- Policy (`reach`, budget, default host) stays per instance. A reach list names instance
  keys, so an instance on another machine needs its own entry.
- Records that arrive before what they depend on are stored rejected and revived. A link
  or an announcement replays every thread, once per batch of appends (`append_all`), so
  the first sync of a home with many agents replays each thread once.

## Possible future improvements

- An offline owner key (RFD 0004 phase 8), with the daily machine linked under it.
- Rex's caller identity as a pairing route, once `axel` can read `server.whoami`.
- Refusing envelopes a revoked device backdates, by periodic signed checkpoints.
- Reach by profile rather than by instance key.

## References

- RFD 0004, "Agents: a profile, and an instance per machine" and "Pairing over a trusted
  route".
- Issue #40.
