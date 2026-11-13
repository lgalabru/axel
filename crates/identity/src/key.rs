//! Ed25519 keypairs and base58 public keys.

use core::fmt;
use core::str::FromStr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use crate::Error;

/// A 32-byte Ed25519 public key.
///
/// Displays and parses as base58 (the Solana address encoding), and
/// serializes the same way.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    /// Wrap raw bytes without checking that they are a valid curve point.
    ///
    /// Validity is checked lazily by [`PublicKey::verify`].
    pub fn from_bytes(bytes: [u8; 32]) -> PublicKey {
        PublicKey(bytes)
    }

    /// The raw 32 bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Base58 encoding of the key (same as `to_string()`).
    pub fn to_base58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }

    /// Parse a base58 string into a key, requiring exactly 32 decoded bytes.
    pub fn from_base58(s: &str) -> Result<PublicKey, Error> {
        let bytes = bs58::decode(s)
            .into_vec()
            .map_err(|e| Error::InvalidPublicKey(format!("{s:?}: {e}")))?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            Error::InvalidPublicKey(format!("{s:?}: expected 32 bytes, got {}", bytes.len()))
        })?;
        Ok(PublicKey(arr))
    }

    /// Verify `sig` over `msg` with strict Ed25519 verification
    /// (`verify_strict`: rejects small-order components and non-canonical
    /// encodings).
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> Result<(), Error> {
        let vk = VerifyingKey::from_bytes(&self.0)
            .map_err(|e| Error::InvalidPublicKey(e.to_string()))?;
        let signature = Signature::from_bytes(sig);
        vk.verify_strict(msg, &signature)
            .map_err(|_| Error::VerificationFailed)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base58())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_base58())
    }
}

impl FromStr for PublicKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PublicKey::from_base58(s)
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        PublicKey::from_base58(&s).map_err(serde::de::Error::custom)
    }
}

impl From<[u8; 32]> for PublicKey {
    fn from(bytes: [u8; 32]) -> Self {
        PublicKey(bytes)
    }
}

/// An Ed25519 signing keypair.
///
/// The secret is held in an `ed25519_dalek::SigningKey`, which zeroizes its
/// memory on drop. `Debug` prints only the public key.
#[derive(Clone)]
pub struct Keypair(SigningKey);

impl Keypair {
    /// Generate a fresh keypair from OS randomness.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot supply randomness. Use
    /// [`Keypair::try_generate`] to handle that case.
    pub fn generate() -> Keypair {
        Keypair::try_generate().expect("OS randomness unavailable")
    }

    /// Generate a fresh keypair, reporting randomness failure as an error.
    pub fn try_generate() -> Result<Keypair, Error> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(|e| Error::Random(e.to_string()))?;
        Ok(Keypair(SigningKey::from_bytes(&seed)))
    }

    /// Rebuild a keypair from its 32-byte secret seed.
    pub fn from_secret_bytes(b: &[u8; 32]) -> Keypair {
        Keypair(SigningKey::from_bytes(b))
    }

    /// The public half.
    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    /// Sign `msg` directly (no pre-hashing).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.0.sign(msg).to_bytes()
    }

    /// The 32-byte secret seed, in a zeroizing wrapper.
    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.0.to_bytes())
    }
}

impl fmt::Debug for Keypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Keypair({})", self.public())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sign_verify_roundtrip() {
        let kp = Keypair::generate();
        let msg = b"hello axel";
        let sig = kp.sign(msg);
        kp.public()
            .verify(msg, &sig)
            .expect("valid signature verifies");
    }

    #[test]
    fn tampered_message_fails() {
        let kp = Keypair::generate();
        let sig = kp.sign(b"hello");
        assert!(matches!(
            kp.public().verify(b"hellp", &sig),
            Err(Error::VerificationFailed)
        ));
    }

    #[test]
    fn tampered_signature_fails() {
        let kp = Keypair::generate();
        let mut sig = kp.sign(b"hello");
        sig[5] ^= 0x01;
        assert!(kp.public().verify(b"hello", &sig).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let a = Keypair::generate();
        let b = Keypair::generate();
        let sig = a.sign(b"hello");
        assert!(b.public().verify(b"hello", &sig).is_err());
    }

    #[test]
    fn two_generated_keys_differ() {
        assert_ne!(Keypair::generate().public(), Keypair::generate().public());
    }

    #[test]
    fn secret_bytes_roundtrip() {
        let kp = Keypair::generate();
        let secret = kp.secret_bytes();
        let again = Keypair::from_secret_bytes(&secret);
        assert_eq!(kp.public(), again.public());
        let sig = again.sign(b"x");
        kp.public().verify(b"x", &sig).unwrap();
    }

    #[test]
    fn base58_display_fromstr_roundtrip() {
        let pk = Keypair::generate().public();
        let s = pk.to_string();
        assert!(s.len() >= 32 && s.len() <= 44, "base58 of 32 bytes: {s}");
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric()));
        let parsed: PublicKey = s.parse().unwrap();
        assert_eq!(parsed, pk);
    }

    #[test]
    fn fromstr_rejects_bad_input() {
        assert!(matches!(
            "0OIl".parse::<PublicKey>(),
            Err(Error::InvalidPublicKey(_))
        ));
        // Valid base58 but wrong length (31 bytes).
        let short = bs58::encode(&[7u8; 31]).into_string();
        assert!(short.parse::<PublicKey>().is_err());
        let long = bs58::encode(&[7u8; 33]).into_string();
        assert!(long.parse::<PublicKey>().is_err());
        assert!("".parse::<PublicKey>().is_err());
    }

    #[test]
    fn serde_as_base58_string() {
        let pk = Keypair::generate().public();
        let json = serde_json::to_string(&pk).unwrap();
        assert_eq!(json, format!("\"{pk}\""));
        let back: PublicKey = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pk);
        assert!(serde_json::from_str::<PublicKey>("\"nope\"").is_err());
        assert!(serde_json::from_str::<PublicKey>("42").is_err());
    }

    #[test]
    fn debug_does_not_leak_secret() {
        let kp = Keypair::generate();
        let dbg = format!("{kp:?}");
        assert!(dbg.contains(&kp.public().to_string()));
        let secret_b58 = bs58::encode(kp.secret_bytes().as_ref()).into_string();
        assert!(!dbg.contains(&secret_b58));
    }

    #[test]
    fn verify_with_garbage_key_and_signature_fails() {
        // Garbage key bytes either fail to decompress (InvalidPublicKey) or
        // decompress to a point that cannot verify (VerificationFailed);
        // both are errors, never a panic.
        for fill in [0x00u8, 0x7f, 0xff] {
            let pk = PublicKey([fill; 32]);
            let sig = [0u8; 64];
            assert!(pk.verify(b"x", &sig).is_err());
        }
    }
}
