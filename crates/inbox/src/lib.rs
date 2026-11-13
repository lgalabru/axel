//! Signed message envelopes for Axel's inbox.
//!
//! Messages between the owner and agents are NIP-01-shaped envelopes signed
//! with Ed25519 (see `docs/DESIGN.md`, "Message envelope"):
//!
//! ```text
//! id  = hex(sha256(canonical_json([0, from_b58, created_at, kind, tags, content])))
//! sig = hex(Ed25519(from_sk, id_bytes))
//! ```
//!
//! This crate provides:
//!
//! - [`kind`]: the message kind constants (`note`, `task`, `permission_request`…).
//! - [`Envelope`] / [`EnvelopeBuilder`]: build, sign and strictly verify
//!   envelopes; read `p` (mention), `t` (thread) and `e` (reply) tags.
//! - [`canonical_bytes`]: the exact bytes that are hashed into an id.
//! - [`mentions`]: `@name` and bare-pubkey mention resolution (ported from
//!   Buzz, minus nostr URIs).
//! - [`threading`]: NIP-10 style `root`/`reply` marker parsing (ported from
//!   Buzz).
//!
//! Pubkeys are base58 strings at this crate's boundary; only signing and
//! verification touch raw bytes via `axel-identity`.

mod envelope;
mod error;
pub mod kind;
pub mod mentions;
pub mod threading;

pub use envelope::{Envelope, EnvelopeBuilder, canonical_bytes, compute_id};
pub use error::Error;
pub use mentions::{MENTION_CAP, Profile, resolve_mentions};
pub use threading::{ThreadMarkers, parse_thread_markers};
