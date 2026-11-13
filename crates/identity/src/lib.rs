//! Ed25519 identities for Axel.
//!
//! Every principal in Axel (the owner and each agent) is an Ed25519 keypair.
//! Public keys cross crate boundaries as base58 strings (the Solana address
//! encoding); signatures are 64 bytes and are always verified with
//! `verify_strict`.
//!
//! This crate provides:
//!
//! - [`Keypair`] / [`PublicKey`]: key generation, signing, strict verification,
//!   base58 encoding.
//! - [`attest()`] / [`verify_attestation`] / [`AuthTag`]: the owner→agent
//!   attestation (adapted from Buzz NIP-OA, re-keyed to Ed25519) and its
//!   conditions mini-language.
//! - [`Keystore`]: secret storage in the OS keychain with an atomic `0600`
//!   JSON file fallback, selectable with `AXEL_KEYSTORE`.
//! - [`hex`]: small lowercase-hex helpers shared with `axel-inbox`.
//!
//! See `docs/DESIGN.md` for the surrounding design.

pub mod attest;
pub mod delegate;
mod error;
pub mod hex;
mod key;
pub mod keystore;

pub use attest::{AuthTag, DOMAIN as ATTESTATION_DOMAIN, attest, verify_attestation};
pub use delegate::{Delegation, delegate};
pub use error::Error;
pub use key::{Keypair, PublicKey};
pub use keystore::Keystore;
