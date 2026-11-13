/// Errors produced by `axel-inbox`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The envelope's `id` does not equal the hash of its canonical form.
    #[error("envelope id mismatch: expected {expected}, got {actual}")]
    IdMismatch {
        /// The id recomputed from the envelope's fields.
        expected: String,
        /// The id carried by the envelope.
        actual: String,
    },

    /// The `sig` field is not 128 hex characters.
    #[error("invalid signature encoding: {0}")]
    InvalidSignature(String),

    /// The `from` field is not a base58 32-byte public key.
    #[error("invalid sender pubkey {0:?}")]
    InvalidPubkey(String),

    /// More than [`crate::MENTION_CAP`] `p` tags.
    #[error("too many mentions: {0} (max {max})", max = crate::MENTION_CAP)]
    TooManyMentions(usize),

    /// Serialization failure while computing the canonical form.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// A key or signature error from `axel-identity`.
    #[error(transparent)]
    Identity(#[from] axel_identity::Error),
}
