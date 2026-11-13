//! Agent → host delegation: an agent lets another key (a remote machine's
//! host key) sign on its behalf, on one thread, for a limited time.
//!
//! This is how an agent works on a remote machine without its private key
//! leaving home.  Envelopes the host signs carry the delegation; replicas
//! that verify it treat the envelope as the agent's (its *author*), while the
//! envelope's `from` stays the host key that actually signed it.
//!
//! # Tag form
//!
//! ```text
//! ["deleg", "<delegator-pubkey-base58>", "<thread-id>", "<conditions>", "<sig-hex>"]
//! ```
//!
//! # Signing
//!
//! ```text
//! preimage = "axel:delegate:v1:" || delegate_pubkey_base58 || ":" || thread_id || ":" || conditions
//! sig      = Ed25519(delegator_sk, preimage)
//! ```
//!
//! `conditions` is the attestation mini-language (see [`crate::attest`]);
//! delegations always bound time (`created_at>…&created_at<…`).

use core::fmt;

use crate::attest::{conditions_permit, validate_conditions};
use crate::hex;
use crate::{Error, Keypair, PublicKey};

/// Domain separator prefixed to every delegation preimage.
pub const DOMAIN: &str = "axel:delegate:v1:";

/// The tag label.
pub const TAG: &str = "deleg";

/// A parsed delegation (the delegate is the envelope's signer, not stored here).
#[derive(Clone, PartialEq, Eq)]
pub struct Delegation {
    /// The agent that delegates.
    pub delegator: PublicKey,
    /// The only thread the delegate may act on.
    pub thread: String,
    /// Conditions string; always carries a time window.
    pub conditions: String,
    /// Ed25519 signature by `delegator` over the preimage.
    pub sig: [u8; 64],
}

impl fmt::Debug for Delegation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delegation")
            .field("delegator", &self.delegator)
            .field("thread", &self.thread)
            .field("conditions", &self.conditions)
            .finish_non_exhaustive()
    }
}

fn preimage(delegate: &PublicKey, thread: &str, conditions: &str) -> Vec<u8> {
    format!("{DOMAIN}{}:{thread}:{conditions}", delegate.to_base58()).into_bytes()
}

/// Let `delegate` sign as `delegator` on `thread` for envelopes created in
/// `(not_before, not_after)` (Unix seconds, exclusive).
pub fn delegate(
    delegator: &Keypair,
    delegate: &PublicKey,
    thread: &str,
    not_before: u64,
    not_after: u64,
) -> Result<Delegation, Error> {
    if delegator.public() == *delegate {
        return Err(Error::InvalidInput("an agent can't delegate to itself".into()));
    }
    if thread.is_empty() || thread.contains(':') || thread.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(Error::InvalidInput(format!("bad thread id {thread:?}")));
    }
    if not_after <= not_before {
        return Err(Error::InvalidInput("the delegation window is empty".into()));
    }
    let conditions = format!("created_at>{not_before}&created_at<{not_after}");
    validate_conditions(&conditions)?;
    let sig = delegator.sign(&preimage(delegate, thread, &conditions));
    Ok(Delegation { delegator: delegator.public(), thread: thread.to_string(), conditions, sig })
}

impl Delegation {
    /// Wire form: `["deleg", delegator_b58, thread, conditions, sig_hex]`.
    pub fn to_tag(&self) -> Vec<String> {
        vec![
            TAG.to_string(),
            self.delegator.to_base58(),
            self.thread.clone(),
            self.conditions.clone(),
            hex::encode(&self.sig),
        ]
    }

    /// Parse the wire form without verifying the signature.
    pub fn from_tag(tag: &[String]) -> Result<Delegation, Error> {
        if tag.len() != 5 || tag[0] != TAG {
            return Err(Error::InvalidInput("a deleg tag has 5 elements and starts with \"deleg\"".into()));
        }
        let delegator = PublicKey::from_base58(&tag[1]).map_err(|e| Error::InvalidInput(format!("delegator pubkey: {e}")))?;
        validate_conditions(&tag[3])?;
        let sig_hex = tag[4].as_str();
        if sig_hex.len() != 128 || !hex::is_lowercase_hex(sig_hex) {
            return Err(Error::InvalidInput("signature must be 128 lowercase hex chars".into()));
        }
        let sig = hex::decode_array::<64>(sig_hex).ok_or_else(|| Error::InvalidSignature("bad hex".into()))?;
        Ok(Delegation { delegator, thread: tag[2].clone(), conditions: tag[3].clone(), sig })
    }

    /// Check the signature for `delegate`, and that an envelope of `kind`
    /// created at `created_at` on `thread` falls inside the delegation.
    pub fn verify_for(&self, delegate: &PublicKey, thread: &str, kind: u32, created_at: u64) -> Result<(), Error> {
        if self.delegator == *delegate {
            return Err(Error::InvalidInput("self-delegation".into()));
        }
        if self.thread != thread {
            return Err(Error::InvalidInput(format!("delegated for thread {}, not {thread}", self.thread)));
        }
        if !self.conditions.contains("created_at<") {
            return Err(Error::InvalidInput("a delegation must expire".into()));
        }
        self.delegator.verify(&preimage(delegate, &self.thread, &self.conditions), &self.sig)?;
        if !conditions_permit(&self.conditions, kind, created_at)? {
            return Err(Error::InvalidInput("outside the delegation's time window".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delegation_verifies_only_for_its_host_thread_and_window() {
        let atlas = Keypair::generate();
        let host = Keypair::generate();
        let d = delegate(&atlas, &host.public(), "t1", 100, 200).unwrap();
        let back = Delegation::from_tag(&d.to_tag()).unwrap();
        assert_eq!(back, d);
        back.verify_for(&host.public(), "t1", 1, 150).unwrap();
        assert!(back.verify_for(&host.public(), "t2", 1, 150).is_err(), "other thread");
        assert!(back.verify_for(&host.public(), "t1", 1, 250).is_err(), "expired");
        assert!(back.verify_for(&host.public(), "t1", 1, 50).is_err(), "too early");
        let other = Keypair::generate();
        assert!(back.verify_for(&other.public(), "t1", 1, 150).is_err(), "another signer");
        let mut forged = back.clone();
        forged.conditions = "created_at>100&created_at<9999".into();
        assert!(forged.verify_for(&host.public(), "t1", 1, 5000).is_err(), "widened window");
    }

    #[test]
    fn bad_delegations_are_refused_at_creation() {
        let a = Keypair::generate();
        assert!(delegate(&a, &a.public(), "t1", 1, 2).is_err());
        assert!(delegate(&a, &Keypair::generate().public(), "t:1", 1, 2).is_err());
        assert!(delegate(&a, &Keypair::generate().public(), "t1", 2, 2).is_err());
    }
}
