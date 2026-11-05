# ADR 0004: Agents are named Ed25519 principals attested by their owner

Status: accepted (retroactive, 2026-10-31). Explains `DESIGN.md` "Principles" 3
and "Identity and crypto".

## Context

In v1 an agent was a config entry; nothing it did could be attributed to it
once the process was gone. Buzz had the right idea: agents as keypairs with an
owner attestation (NIP-OA), an inbox defined by `p` tags, and a keystore
pattern that refuses to spawn without a key (`RESEARCH.md` "Buzz"). Buzz uses
secp256k1/BIP-340 because it rides on Nostr relays. We have no relay and no
Nostr interop goal.

## Decision

An agent is a named identity with an Ed25519 keypair, attested by the owner's
key (`axel:agent-auth:v1:` preimage, Buzz's conditions mini-language ported
verbatim), addressable as `@name`, with an inbox. Identity outlives processes
and threads. Keys are 32 bytes shown as base58 (so an agent key is also a
valid Solana address), stored in the OS keychain with a `0600` file fallback.
Private keys never leave the machine that made them; work on a host runs under
a short-lived job key with a signed delegation.

## Consequences

- Every envelope is signed, so replicas can merge without trusting each other
  (ADR 0005) and a host can't forge its laptop's agents.
- No Nostr interop; Buzz modules were ported, not linked (`IMPORTS.md`).
- Encryption is out of scope for now; when needed, a separate X25519 key per
  identity rather than converting the Ed25519 key.
- Launching an agent whose key is missing is refused, by design.
