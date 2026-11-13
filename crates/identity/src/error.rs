/// Errors produced by `axel-identity`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A public key was not 32 bytes or was not valid base58.
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),

    /// A signature was malformed (wrong length, bad hex).
    #[error("invalid signature: {0}")]
    InvalidSignature(String),

    /// The signature did not verify (strict Ed25519 verification).
    #[error("signature verification failed")]
    VerificationFailed,

    /// Structurally invalid input (conditions, tag shape, self-attestation…).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The operating system could not supply randomness.
    #[error("randomness unavailable: {0}")]
    Random(String),

    /// The keystore contents were corrupt or inconsistent.
    #[error("keystore: {0}")]
    Keystore(String),

    /// The OS keychain backend failed.
    #[error("keychain: {0}")]
    Keychain(String),

    /// A file-backend I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
