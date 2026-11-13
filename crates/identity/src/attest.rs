//! Owner → agent attestation.
//!
//! Adapted from Buzz's NIP-OA (`nip_oa.rs`), re-keyed from secp256k1 Schnorr
//! to Ed25519. An attestation proves that the owner key authorised an agent
//! key to act (publish messages) under the agent's own authorship.
//!
//! # Tag form
//!
//! ```text
//! ["auth", "<owner-pubkey-base58>", "<conditions>", "<sig-hex>"]
//! ```
//!
//! # Signing
//!
//! ```text
//! preimage = "axel:agent-auth:v1:" || agent_pubkey_base58 || ":" || conditions
//! sig      = Ed25519(owner_sk, preimage)       // signed directly, no pre-hash
//! ```
//!
//! # Conditions mini-language
//!
//! Empty string means "unconditional". Otherwise one or more clauses joined
//! with `&`, each being `kind=<0-65535>`, `created_at<<0-4294967295>` or
//! `created_at><0-4294967295>`. Values are canonical decimals (no leading
//! zeros, no sign, no whitespace). Ported verbatim from Buzz.

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::hex;
use crate::{Error, Keypair, PublicKey};

/// Domain separator prefixed to every attestation preimage.
pub const DOMAIN: &str = "axel:agent-auth:v1:";

/// A parsed owner attestation.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthTag {
    /// The owner (attesting) public key.
    pub owner: PublicKey,
    /// Conditions string (see module docs). Empty means unconditional.
    pub conditions: String,
    /// Ed25519 signature by `owner` over the preimage.
    pub sig: [u8; 64],
}

impl fmt::Debug for AuthTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthTag")
            .field("owner", &self.owner)
            .field("conditions", &self.conditions)
            .field("sig", &hex::encode(&self.sig))
            .finish()
    }
}

impl AuthTag {
    /// Wire form: `["auth", owner_b58, conditions, sig_hex]`.
    pub fn to_tag(&self) -> Vec<String> {
        vec![
            "auth".to_string(),
            self.owner.to_base58(),
            self.conditions.clone(),
            hex::encode(&self.sig),
        ]
    }

    /// Parse the wire form without verifying the signature.
    ///
    /// Validates structure only: exactly four elements, label `"auth"`, a
    /// base58 32-byte owner key, valid conditions, and a 128-character
    /// lowercase-hex signature.
    pub fn from_tag(tag: &[String]) -> Result<AuthTag, Error> {
        if tag.len() != 4 {
            return Err(Error::InvalidInput(format!(
                "auth tag must have 4 elements, got {}",
                tag.len()
            )));
        }
        if tag[0] != "auth" {
            return Err(Error::InvalidInput(format!(
                "first element must be \"auth\", got {:?}",
                tag[0]
            )));
        }
        let owner = PublicKey::from_base58(&tag[1])
            .map_err(|e| Error::InvalidInput(format!("owner pubkey: {e}")))?;
        validate_conditions(&tag[2])?;
        let sig_hex = tag[3].as_str();
        if sig_hex.len() != 128 || !hex::is_lowercase_hex(sig_hex) {
            return Err(Error::InvalidInput(format!(
                "signature must be 128 lowercase hex chars, got length {}",
                sig_hex.len()
            )));
        }
        let sig = hex::decode_array::<64>(sig_hex)
            .ok_or_else(|| Error::InvalidSignature("bad hex".into()))?;
        Ok(AuthTag {
            owner,
            conditions: tag[2].clone(),
            sig,
        })
    }

    /// Evaluate the conditions against a concrete message.
    ///
    /// `kind=N` requires `kind == N`; `created_at<T` and `created_at>T` are
    /// strict. Every clause must pass. Does **not** check the signature; call
    /// [`verify_attestation`] for that (or [`verify_attestation_for`] for both).
    pub fn permits(&self, kind: u32, created_at: u64) -> Result<bool, Error> {
        conditions_permit(&self.conditions, kind, created_at)
    }
}

impl Serialize for AuthTag {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_tag().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AuthTag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let parts = Vec::<String>::deserialize(deserializer)?;
        AuthTag::from_tag(&parts).map_err(serde::de::Error::custom)
    }
}

/// Build the signing preimage for `agent` under `conditions`.
pub fn preimage(agent: &PublicKey, conditions: &str) -> Vec<u8> {
    format!("{DOMAIN}{}:{conditions}", agent.to_base58()).into_bytes()
}

/// Validate a `conditions` string.
///
/// Empty is valid. Non-empty must be `clause` or `clause&clause&...` where
/// each clause is `kind=<0-65535>`, `created_at<<0-4294967295>` or
/// `created_at><0-4294967295>`, canonical decimals only.
pub fn validate_conditions(conditions: &str) -> Result<(), Error> {
    if conditions.is_empty() {
        return Ok(());
    }
    if conditions.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(Error::InvalidInput(
            "conditions must not contain whitespace".into(),
        ));
    }
    for clause in conditions.split('&') {
        if clause.is_empty() {
            return Err(Error::InvalidInput(
                "empty clause in conditions (leading/trailing/double '&')".into(),
            ));
        }
        validate_clause(clause)?;
    }
    Ok(())
}

fn validate_clause(clause: &str) -> Result<(), Error> {
    if let Some(value) = clause.strip_prefix("kind=") {
        validate_canonical_decimal(value, 0, 65535, "kind")
    } else if let Some(value) = clause.strip_prefix("created_at<") {
        validate_canonical_decimal(value, 0, 4_294_967_295, "created_at<")
    } else if let Some(value) = clause.strip_prefix("created_at>") {
        validate_canonical_decimal(value, 0, 4_294_967_295, "created_at>")
    } else {
        Err(Error::InvalidInput(format!(
            "unsupported clause: {clause:?}"
        )))
    }
}

fn validate_canonical_decimal(s: &str, min: u64, max: u64, label: &str) -> Result<(), Error> {
    if s.is_empty() {
        return Err(Error::InvalidInput(format!(
            "{label} value must not be empty"
        )));
    }
    if s.len() > 1 && s.starts_with('0') {
        return Err(Error::InvalidInput(format!(
            "{label} value has leading zero: {s:?}"
        )));
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::InvalidInput(format!(
            "{label} value is not a valid decimal: {s:?}"
        )));
    }
    let value: u64 = s
        .parse()
        .map_err(|e| Error::InvalidInput(format!("{label} value out of range: {e}")))?;
    if value < min || value > max {
        return Err(Error::InvalidInput(format!(
            "{label} value {value} out of range [{min}, {max}]"
        )));
    }
    Ok(())
}

/// Evaluate a (valid) conditions string against a message's kind and time.
///
/// Returns an error if the conditions string itself is invalid.
pub fn conditions_permit(conditions: &str, kind: u32, created_at: u64) -> Result<bool, Error> {
    validate_conditions(conditions)?;
    if conditions.is_empty() {
        return Ok(true);
    }
    for clause in conditions.split('&') {
        let ok = if let Some(v) = clause.strip_prefix("kind=") {
            parse_bound(v)? == u64::from(kind)
        } else if let Some(v) = clause.strip_prefix("created_at<") {
            created_at < parse_bound(v)?
        } else if let Some(v) = clause.strip_prefix("created_at>") {
            created_at > parse_bound(v)?
        } else {
            return Err(Error::InvalidInput(format!(
                "unsupported clause: {clause:?}"
            )));
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn parse_bound(v: &str) -> Result<u64, Error> {
    v.parse::<u64>()
        .map_err(|e| Error::InvalidInput(format!("invalid bound {v:?}: {e}")))
}

/// Sign an attestation of `agent` by `owner` under `conditions`.
///
/// # Errors
///
/// - `owner.public() == *agent` (self-attestation is meaningless).
/// - Invalid `conditions`.
pub fn attest(owner: &Keypair, agent: &PublicKey, conditions: &str) -> Result<AuthTag, Error> {
    let owner_pk = owner.public();
    if owner_pk == *agent {
        return Err(Error::InvalidInput(
            "owner and agent pubkeys must differ (self-attestation rejected)".into(),
        ));
    }
    validate_conditions(conditions)?;
    let sig = owner.sign(&preimage(agent, conditions));
    Ok(AuthTag {
        owner: owner_pk,
        conditions: conditions.to_string(),
        sig,
    })
}

/// Verify that `tag` is a valid attestation of `agent` by `tag.owner`.
///
/// Checks: owner != agent, conditions are well-formed, and the signature
/// verifies (strictly) over the reconstructed preimage. Conditions are not
/// evaluated; see [`verify_attestation_for`].
pub fn verify_attestation(tag: &AuthTag, agent: &PublicKey) -> Result<(), Error> {
    if tag.owner == *agent {
        return Err(Error::InvalidInput(
            "owner and agent pubkeys must differ (self-attestation rejected)".into(),
        ));
    }
    validate_conditions(&tag.conditions)?;
    tag.owner
        .verify(&preimage(agent, &tag.conditions), &tag.sig)
}

/// Verify the attestation *and* that its conditions permit a message of
/// `kind` created at `created_at`.
pub fn verify_attestation_for(
    tag: &AuthTag,
    agent: &PublicKey,
    kind: u32,
    created_at: u64,
) -> Result<(), Error> {
    verify_attestation(tag, agent)?;
    if tag.permits(kind, created_at)? {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "message (kind={kind}, created_at={created_at}) does not satisfy conditions {:?}",
            tag.conditions
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONDITIONS: &str = "kind=1&created_at<1713957000";

    fn pair() -> (Keypair, Keypair) {
        (Keypair::generate(), Keypair::generate())
    }

    #[test]
    fn preimage_has_domain_and_separator() {
        let agent = Keypair::generate().public();
        let p = preimage(&agent, CONDITIONS);
        assert_eq!(
            String::from_utf8(p).unwrap(),
            format!("axel:agent-auth:v1:{}:{}", agent, CONDITIONS)
        );
        // Empty conditions still has the trailing ':'.
        let p = preimage(&agent, "");
        assert!(String::from_utf8(p).unwrap().ends_with(':'));
    }

    #[test]
    fn attest_then_verify_roundtrip() {
        let (owner, agent) = pair();
        let tag = attest(&owner, &agent.public(), "kind=9").unwrap();
        assert_eq!(tag.owner, owner.public());
        assert_eq!(tag.conditions, "kind=9");
        verify_attestation(&tag, &agent.public()).unwrap();
    }

    #[test]
    fn empty_conditions_roundtrip() {
        let (owner, agent) = pair();
        let tag = attest(&owner, &agent.public(), "").unwrap();
        verify_attestation(&tag, &agent.public()).unwrap();
    }

    #[test]
    fn tag_wire_roundtrip() {
        let (owner, agent) = pair();
        let tag = attest(&owner, &agent.public(), CONDITIONS).unwrap();
        let wire = tag.to_tag();
        assert_eq!(wire.len(), 4);
        assert_eq!(wire[0], "auth");
        assert_eq!(wire[1], owner.public().to_string());
        assert_eq!(wire[2], CONDITIONS);
        assert_eq!(wire[3].len(), 128);
        assert!(hex::is_lowercase_hex(&wire[3]));
        let parsed = AuthTag::from_tag(&wire).unwrap();
        assert_eq!(parsed, tag);
        verify_attestation(&parsed, &agent.public()).unwrap();
    }

    #[test]
    fn serde_roundtrip_is_tag_array() {
        let (owner, agent) = pair();
        let tag = attest(&owner, &agent.public(), "kind=2").unwrap();
        let json = serde_json::to_string(&tag).unwrap();
        assert!(json.starts_with("[\"auth\","));
        let back: AuthTag = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn wrong_agent_fails() {
        let (owner, agent) = pair();
        let other = Keypair::generate().public();
        let tag = attest(&owner, &agent.public(), "").unwrap();
        assert!(matches!(
            verify_attestation(&tag, &other),
            Err(Error::VerificationFailed)
        ));
    }

    #[test]
    fn tampered_conditions_fail() {
        let (owner, agent) = pair();
        let mut tag = attest(&owner, &agent.public(), "kind=1").unwrap();
        tag.conditions = "kind=2".into();
        assert!(matches!(
            verify_attestation(&tag, &agent.public()),
            Err(Error::VerificationFailed)
        ));
    }

    #[test]
    fn tampered_signature_fails() {
        let (owner, agent) = pair();
        let mut tag = attest(&owner, &agent.public(), "kind=1").unwrap();
        tag.sig[0] ^= 1;
        assert!(verify_attestation(&tag, &agent.public()).is_err());
    }

    #[test]
    fn swapped_owner_fails() {
        let (owner, agent) = pair();
        let mut tag = attest(&owner, &agent.public(), "kind=1").unwrap();
        tag.owner = Keypair::generate().public();
        assert!(verify_attestation(&tag, &agent.public()).is_err());
    }

    #[test]
    fn reject_self_attestation() {
        let kp = Keypair::generate();
        let err = attest(&kp, &kp.public(), "kind=9").unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err:?}");

        // Crafted self-attesting tag with a (necessarily bogus) signature.
        let tag = AuthTag {
            owner: kp.public(),
            conditions: "kind=9".into(),
            sig: [0xaa; 64],
        };
        assert!(matches!(
            verify_attestation(&tag, &kp.public()),
            Err(Error::InvalidInput(_))
        ));
        // Even a correctly signed self-attestation is rejected.
        let tag = AuthTag {
            owner: kp.public(),
            conditions: "".into(),
            sig: kp.sign(&preimage(&kp.public(), "")),
        };
        assert!(matches!(
            verify_attestation(&tag, &kp.public()),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn from_tag_rejects_malformed() {
        let (owner, agent) = pair();
        let good = attest(&owner, &agent.public(), CONDITIONS)
            .unwrap()
            .to_tag();
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert!(AuthTag::from_tag(&[]).is_err());
        assert!(AuthTag::from_tag(&good[..3]).is_err());
        let mut five = good.clone();
        five.push("x".into());
        assert!(AuthTag::from_tag(&five).is_err());

        let mut bad_label = good.clone();
        bad_label[0] = "notauth".into();
        assert!(AuthTag::from_tag(&bad_label).is_err());

        assert!(AuthTag::from_tag(&s(&["auth", "notakey", CONDITIONS, &"a".repeat(128)])).is_err());
        let short_pk = bs58::encode(&[1u8; 31]).into_string();
        assert!(AuthTag::from_tag(&s(&["auth", &short_pk, CONDITIONS, &"a".repeat(128)])).is_err());

        assert!(AuthTag::from_tag(&s(&["auth", &good[1], CONDITIONS, "abcd"])).is_err());
        assert!(AuthTag::from_tag(&s(&["auth", &good[1], CONDITIONS, &"z".repeat(128)])).is_err());
        // Uppercase hex is non-canonical and rejected.
        let upper = good[3].to_uppercase();
        assert!(AuthTag::from_tag(&s(&["auth", &good[1], CONDITIONS, &upper])).is_err());
        // Invalid conditions rejected at parse.
        assert!(AuthTag::from_tag(&s(&["auth", &good[1], "kind=01", &good[3]])).is_err());
        assert!(AuthTag::from_tag(&s(&["auth", &good[1], "kind=1&", &good[3]])).is_err());

        // Structurally valid but wrong signature parses, then fails verify.
        let wrong =
            AuthTag::from_tag(&s(&["auth", &good[1], CONDITIONS, &"0".repeat(128)])).unwrap();
        assert!(verify_attestation(&wrong, &agent.public()).is_err());
    }

    #[test]
    fn valid_conditions() {
        assert!(validate_conditions("").is_ok());
        assert!(validate_conditions("kind=1").is_ok());
        assert!(validate_conditions("kind=0").is_ok());
        assert!(validate_conditions("kind=65535").is_ok());
        assert!(validate_conditions("created_at<1713957000").is_ok());
        assert!(validate_conditions("created_at>0").is_ok());
        assert!(validate_conditions("created_at>4294967295").is_ok());
        assert!(validate_conditions("kind=1&created_at<1713957000").is_ok());
        assert!(validate_conditions("kind=9&created_at>100&created_at<200").is_ok());
    }

    #[test]
    fn reject_ampersand_misuse() {
        assert!(matches!(
            validate_conditions("kind=1&").unwrap_err(),
            Error::InvalidInput(_)
        ));
        assert!(validate_conditions("&kind=1").is_err());
        assert!(validate_conditions("kind=1&&created_at<100").is_err());
    }

    #[test]
    fn reject_leading_zero() {
        assert!(validate_conditions("kind=01").is_err());
        assert!(validate_conditions("created_at<01713957000").is_err());
    }

    #[test]
    fn reject_whitespace() {
        assert!(validate_conditions("kind=1 ").is_err());
        assert!(validate_conditions(" kind=1").is_err());
        assert!(validate_conditions("kind= 1").is_err());
        assert!(validate_conditions("kind=1\t").is_err());
    }

    #[test]
    fn reject_unknown_clause() {
        assert!(validate_conditions("foo=1").is_err());
        assert!(validate_conditions("Kind=1").is_err());
        assert!(validate_conditions("CREATED_AT<100").is_err());
        assert!(validate_conditions("created_at=100").is_err());
    }

    #[test]
    fn reject_out_of_range() {
        assert!(validate_conditions("kind=65536").is_err());
        assert!(validate_conditions("created_at<4294967296").is_err());
        assert!(validate_conditions("kind=99999999999999999999999").is_err());
    }

    #[test]
    fn reject_non_decimal() {
        assert!(validate_conditions("kind=abc").is_err());
        assert!(validate_conditions("kind=-1").is_err());
        assert!(validate_conditions("kind=1.0").is_err());
        assert!(validate_conditions("kind=+1").is_err());
    }

    #[test]
    fn reject_empty_value() {
        assert!(validate_conditions("kind=").is_err());
        assert!(validate_conditions("created_at<").is_err());
        assert!(validate_conditions("created_at>").is_err());
    }

    #[test]
    fn attest_rejects_invalid_conditions() {
        let (owner, agent) = pair();
        assert!(matches!(
            attest(&owner, &agent.public(), "kind=01").unwrap_err(),
            Error::InvalidInput(_)
        ));
    }

    #[test]
    fn verify_rejects_invalid_conditions_even_with_valid_shape() {
        let (owner, agent) = pair();
        let tag = AuthTag {
            owner: owner.public(),
            conditions: "kind=01".into(),
            sig: owner.sign(&preimage(&agent.public(), "kind=01")),
        };
        assert!(matches!(
            verify_attestation(&tag, &agent.public()),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn conditions_are_evaluated_strictly() {
        let (owner, agent) = pair();
        let a = agent.public();

        let expired = attest(&owner, &a, "created_at<1").unwrap();
        assert!(verify_attestation_for(&expired, &a, 1, 200).is_err());
        assert!(!expired.permits(1, 200).unwrap());

        let not_yet = attest(&owner, &a, "created_at>200").unwrap();
        assert!(verify_attestation_for(&not_yet, &a, 1, 200).is_err());

        let second_fails = attest(&owner, &a, "created_at<201&created_at<200").unwrap();
        assert!(verify_attestation_for(&second_fails, &a, 1, 200).is_err());

        let in_window = attest(&owner, &a, "kind=9&created_at>199&created_at<201").unwrap();
        verify_attestation_for(&in_window, &a, 9, 200).unwrap();
        assert!(verify_attestation_for(&in_window, &a, 10, 200).is_err());

        let unconditional = attest(&owner, &a, "").unwrap();
        verify_attestation_for(&unconditional, &a, 12345, 0).unwrap();
    }

    #[test]
    fn verify_for_still_checks_signature() {
        let (owner, agent) = pair();
        let mut tag = attest(&owner, &agent.public(), "kind=1").unwrap();
        tag.sig[63] ^= 0x80;
        assert!(verify_attestation_for(&tag, &agent.public(), 1, 0).is_err());
    }
}
