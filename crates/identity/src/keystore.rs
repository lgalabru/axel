//! Secret storage for the owner key and agent keys.
//!
//! Two backends:
//!
//! - **Keychain**: the OS keychain via the `keyring` crate, service `axel`,
//!   user `secrets`. All secrets live in one JSON blob so the OS prompts at
//!   most once per process.
//! - **File**: `~/.axel/secrets.json` (or any path given to
//!   [`Keystore::open_at`]), written atomically (temp file + rename) with mode
//!   `0600`. Used when the keychain is unavailable, and by tests.
//!
//! Blob shape:
//!
//! ```json
//! { "owner": "<base58 secret>", "agents": { "<pubkey b58>": "<base58 secret>" } }
//! ```
//!
//! Secrets are base58-encoded 32-byte Ed25519 seeds. Every write is followed
//! by a read-back verification. In-memory copies of the blob are zeroized.
//!
//! # Environment
//!
//! - `AXEL_KEYSTORE=file` forces the file backend; `AXEL_KEYSTORE=keychain`
//!   forces the keychain (erroring if it is unusable); unset or empty tries
//!   the keychain first and falls back to the file. Dev builds and smoke
//!   tests set `file` so each rebuilt binary does not re-prompt macOS.
//! - `AXEL_HOME` overrides the `~/.axel` directory holding `secrets.json`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::{Error, Keypair, PublicKey};

const KEYCHAIN_SERVICE: &str = "axel";
const KEYCHAIN_USER: &str = "secrets";

/// Env var selecting the backend: `file`, `keychain`, or unset (auto).
pub const BACKEND_ENV: &str = "AXEL_KEYSTORE";
/// Env var overriding the `~/.axel` directory.
pub const HOME_ENV: &str = "AXEL_HOME";

enum Backend {
    Keychain(keyring::Entry),
    File(PathBuf),
}

/// The owner's and agents' secret keys, in the keychain or a `0600` file.
pub struct Keystore {
    backend: Backend,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.backend {
            Backend::Keychain(_) => f.write_str("Keystore(keychain)"),
            Backend::File(p) => write!(f, "Keystore(file: {})", p.display()),
        }
    }
}

/// The on-disk / in-keychain JSON blob. Zeroized on drop.
#[derive(Default, Serialize, Deserialize)]
struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default)]
    agents: BTreeMap<String, String>,
}

impl Drop for Secrets {
    fn drop(&mut self) {
        if let Some(o) = self.owner.as_mut() {
            o.zeroize();
        }
        for v in self.agents.values_mut() {
            v.zeroize();
        }
    }
}

fn encode_secret(kp: &Keypair) -> Zeroizing<String> {
    Zeroizing::new(bs58::encode(kp.secret_bytes().as_ref()).into_string())
}

fn decode_secret(s: &str, what: &str) -> Result<Keypair, Error> {
    let bytes = Zeroizing::new(
        bs58::decode(s)
            .into_vec()
            .map_err(|e| Error::Keystore(format!("{what}: secret is not base58: {e}")))?,
    );
    if bytes.len() != 32 {
        return Err(Error::Keystore(format!(
            "{what}: secret must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut seed = Zeroizing::new([0u8; 32]);
    seed.copy_from_slice(&bytes);
    Ok(Keypair::from_secret_bytes(&seed))
}

impl Keystore {
    /// Default fallback file: `$AXEL_HOME/secrets.json`, or
    /// `~/.axel/secrets.json` when `AXEL_HOME` is unset.
    pub fn default_file_path() -> PathBuf {
        let home = match std::env::var_os(HOME_ENV) {
            Some(h) if !h.is_empty() => PathBuf::from(h),
            _ => dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".axel"),
        };
        home.join("secrets.json")
    }

    /// Open the default keystore, honouring `AXEL_KEYSTORE`:
    ///
    /// - `file`: the `0600` file at [`Keystore::default_file_path`].
    /// - `keychain`: the OS keychain; errors if it cannot be reached.
    /// - unset/empty: the keychain if it answers, otherwise the file.
    pub fn open() -> Result<Keystore, Error> {
        let path = Keystore::default_file_path();
        let selector = std::env::var(BACKEND_ENV).unwrap_or_default();
        match selector.trim().to_ascii_lowercase().as_str() {
            // Default is the 0600 file (like ssh keys): the OS keychain re-prompts
            // whenever the binary changes, which is constantly during development.
            "" | "file" => Keystore::open_at(path, false),
            "auto" => Keystore::open_at(path, true),
            "keychain" => {
                let entry = probe_keychain().ok_or_else(|| {
                    Error::Keychain(format!(
                        "{BACKEND_ENV}=keychain but the OS keychain is unusable"
                    ))
                })?;
                Ok(Keystore {
                    backend: Backend::Keychain(entry),
                })
            }
            other => Err(Error::InvalidInput(format!(
                "{BACKEND_ENV} must be \"file\", \"keychain\" or unset, got {other:?}"
            ))),
        }
    }

    /// Open a keystore at an explicit fallback path.
    ///
    /// With `use_keychain = true` the keychain is probed first and the file
    /// is only used if the keychain errors. With `false` the file is used
    /// directly (what tests do; the keychain is never touched).
    pub fn open_at(fallback_file: PathBuf, use_keychain: bool) -> Result<Keystore, Error> {
        if use_keychain && let Some(entry) = probe_keychain() {
            return Ok(Keystore {
                backend: Backend::Keychain(entry),
            });
        }
        Ok(Keystore {
            backend: Backend::File(fallback_file),
        })
    }

    /// `"keychain"` or `"file"`.
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Keychain(_) => "keychain",
            Backend::File(_) => "file",
        }
    }

    /// The fallback file path when the file backend is active.
    pub fn file_path(&self) -> Option<&Path> {
        match &self.backend {
            Backend::Keychain(_) => None,
            Backend::File(p) => Some(p),
        }
    }

    /// The owner keypair, generated and stored on first call.
    pub fn owner(&self) -> Result<Keypair, Error> {
        let mut secrets = self.load()?;
        if let Some(enc) = secrets.owner.as_deref() {
            return decode_secret(enc, "owner");
        }
        let kp = Keypair::try_generate()?;
        secrets.owner = Some(encode_secret(&kp).to_string());
        self.save(&secrets)?;
        drop(secrets);
        // Read-back verify.
        let again = self.load()?;
        let stored = again
            .owner
            .as_deref()
            .ok_or_else(|| Error::Keystore("owner missing after write".into()))?;
        let back = decode_secret(stored, "owner")?;
        if back.public() != kp.public() {
            return Err(Error::Keystore("owner read-back mismatch".into()));
        }
        Ok(kp)
    }

    /// The owner's public key without creating one. `None` until
    /// [`Keystore::owner`] has run once.
    pub fn owner_public(&self) -> Result<Option<PublicKey>, Error> {
        let secrets = self.load()?;
        match secrets.owner.as_deref() {
            Some(enc) => Ok(Some(decode_secret(enc, "owner")?.public())),
            None => Ok(None),
        }
    }

    /// Store an agent keypair under its public key, then read it back and
    /// check it matches.
    ///
    /// Storing a key that is already present and identical is a no-op
    /// success. Storing a *different* secret under an existing public key
    /// cannot happen for valid Ed25519 keys (the public key is derived), but
    /// a corrupt existing entry is overwritten.
    pub fn put_agent(&self, kp: &Keypair) -> Result<(), Error> {
        let pk = kp.public().to_base58();
        let encoded = encode_secret(kp);
        let mut secrets = self.load()?;
        if secrets
            .agents
            .get(&pk)
            .is_some_and(|existing| *existing == *encoded)
        {
            return Ok(());
        }
        secrets.agents.insert(pk.clone(), encoded.to_string());
        self.save(&secrets)?;
        drop(secrets);
        let again = self.load()?;
        let stored = again
            .agents
            .get(&pk)
            .ok_or_else(|| Error::Keystore(format!("agent {pk} missing after write")))?;
        let back = decode_secret(stored, &format!("agent {pk}"))?;
        if back.public() != kp.public() {
            return Err(Error::Keystore(format!("agent {pk} read-back mismatch")));
        }
        Ok(())
    }

    /// Fetch an agent keypair, `None` if absent.
    ///
    /// Errors if the stored secret is corrupt or does not derive `pk`.
    pub fn agent(&self, pk: &PublicKey) -> Result<Option<Keypair>, Error> {
        let key = pk.to_base58();
        let secrets = self.load()?;
        match secrets.agents.get(&key) {
            None => Ok(None),
            Some(enc) => {
                let kp = decode_secret(enc, &format!("agent {key}"))?;
                if kp.public() != *pk {
                    return Err(Error::Keystore(format!(
                        "agent {key}: stored secret derives a different public key"
                    )));
                }
                Ok(Some(kp))
            }
        }
    }

    /// Remove an agent's secret. Removing an absent agent is not an error.
    pub fn remove_agent(&self, pk: &PublicKey) -> Result<(), Error> {
        let key = pk.to_base58();
        let mut secrets = self.load()?;
        if secrets.agents.remove(&key).is_some() {
            self.save(&secrets)?;
        }
        Ok(())
    }

    /// Public keys of every stored agent, sorted by base58.
    pub fn list_agents(&self) -> Result<Vec<PublicKey>, Error> {
        let secrets = self.load()?;
        secrets
            .agents
            .keys()
            .map(|k| {
                PublicKey::from_base58(k)
                    .map_err(|e| Error::Keystore(format!("bad agent key {k:?}: {e}")))
            })
            .collect()
    }

    fn load(&self) -> Result<Secrets, Error> {
        let raw: Option<Zeroizing<String>> = match &self.backend {
            Backend::Keychain(entry) => match entry.get_password() {
                Ok(s) => Some(Zeroizing::new(s)),
                Err(keyring::Error::NoEntry) => None,
                Err(e) => return Err(Error::Keychain(e.to_string())),
            },
            Backend::File(path) => match fs::read(path) {
                Ok(bytes) => {
                    let bytes = Zeroizing::new(bytes);
                    let s = String::from_utf8(bytes.to_vec())
                        .map_err(|e| Error::Keystore(format!("secrets file is not UTF-8: {e}")))?;
                    Some(Zeroizing::new(s))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(Error::Io(e)),
            },
        };
        match raw {
            None => Ok(Secrets::default()),
            Some(s) if s.trim().is_empty() => Ok(Secrets::default()),
            Some(s) => Ok(serde_json::from_str(&s)?),
        }
    }

    fn save(&self, secrets: &Secrets) -> Result<(), Error> {
        let json = Zeroizing::new(serde_json::to_string(secrets)?);
        match &self.backend {
            Backend::Keychain(entry) => entry
                .set_password(&json)
                .map_err(|e| Error::Keychain(e.to_string())),
            Backend::File(path) => write_atomic_0600(path, json.as_bytes()),
        }
    }
}

/// Probe the keychain. Returns the entry if the keychain is usable (an
/// existing blob, or a clean "no entry"), `None` on any other failure.
fn probe_keychain() -> Option<keyring::Entry> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).ok()?;
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => Some(entry),
        Err(_) => None,
    }
}

/// Write `data` to `path` via a sibling temp file and rename, with the file
/// (and any created parent directories) restricted to the current user.
fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<(), Error> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    create_private_dir(&dir)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("secrets.json");
    let nonce = getrandom::u32().map_err(|e| Error::Random(e.to_string()))?;
    let tmp = dir.join(format!(
        ".{file_name}.{}.{nonce:08x}.tmp",
        std::process::id()
    ));

    let result = (|| -> Result<(), Error> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Belt and braces: the create mode is subject to umask.
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_private_dir(dir: &Path) -> Result<(), Error> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Keystore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("secrets.json");
        let ks = Keystore::open_at(path, false).unwrap();
        (dir, ks)
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn file_backend_is_used_when_keychain_disabled() {
        let (_dir, ks) = temp_store();
        assert_eq!(ks.backend_name(), "file");
        assert!(ks.file_path().is_some());
        assert!(format!("{ks:?}").starts_with("Keystore(file"));
    }

    #[test]
    fn owner_is_created_once_and_stable() {
        let (_dir, ks) = temp_store();
        assert_eq!(ks.owner_public().unwrap(), None);
        let a = ks.owner().unwrap();
        let b = ks.owner().unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(ks.owner_public().unwrap(), Some(a.public()));

        // A second handle to the same file sees the same owner.
        let ks2 = Keystore::open_at(ks.file_path().unwrap().to_path_buf(), false).unwrap();
        assert_eq!(ks2.owner().unwrap().public(), a.public());
    }

    #[test]
    fn agent_roundtrip_and_remove() {
        let (_dir, ks) = temp_store();
        let kp = Keypair::generate();
        let pk = kp.public();
        assert!(ks.agent(&pk).unwrap().is_none());

        ks.put_agent(&kp).unwrap();
        let back = ks.agent(&pk).unwrap().expect("stored");
        assert_eq!(back.public(), pk);
        assert_eq!(*back.secret_bytes(), *kp.secret_bytes());
        assert_eq!(ks.list_agents().unwrap(), vec![pk]);

        // Idempotent re-put.
        ks.put_agent(&kp).unwrap();
        assert_eq!(ks.list_agents().unwrap().len(), 1);

        ks.remove_agent(&pk).unwrap();
        assert!(ks.agent(&pk).unwrap().is_none());
        assert!(ks.list_agents().unwrap().is_empty());
        // Removing again is fine.
        ks.remove_agent(&pk).unwrap();
    }

    #[test]
    fn multiple_agents_and_owner_coexist() {
        let (_dir, ks) = temp_store();
        let owner = ks.owner().unwrap();
        let a = Keypair::generate();
        let b = Keypair::generate();
        ks.put_agent(&a).unwrap();
        ks.put_agent(&b).unwrap();
        assert_eq!(ks.owner().unwrap().public(), owner.public());
        let mut listed = ks.list_agents().unwrap();
        listed.sort();
        let mut expected = vec![a.public(), b.public()];
        expected.sort();
        assert_eq!(listed, expected);
        ks.remove_agent(&a.public()).unwrap();
        assert_eq!(ks.list_agents().unwrap(), vec![b.public()]);
        assert_eq!(ks.owner().unwrap().public(), owner.public());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_0600_and_dir_is_0700() {
        let (_dir, ks) = temp_store();
        ks.owner().unwrap();
        let path = ks.file_path().unwrap();
        assert_eq!(mode_of(path), 0o600);
        assert_eq!(mode_of(path.parent().unwrap()), 0o700);
        // Still 0600 after a rewrite.
        ks.put_agent(&Keypair::generate()).unwrap();
        assert_eq!(mode_of(path), 0o600);
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn blob_has_expected_shape() {
        let (_dir, ks) = temp_store();
        let owner = ks.owner().unwrap();
        let agent = Keypair::generate();
        ks.put_agent(&agent).unwrap();
        let raw = fs::read_to_string(ks.file_path().unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let owner_b58 = bs58::encode(owner.secret_bytes().as_ref()).into_string();
        assert_eq!(v["owner"], owner_b58);
        let agent_b58 = bs58::encode(agent.secret_bytes().as_ref()).into_string();
        assert_eq!(v["agents"][agent.public().to_string()], agent_b58);
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let (_dir, ks) = temp_store();
        assert!(ks.list_agents().unwrap().is_empty());
        assert!(ks.agent(&Keypair::generate().public()).unwrap().is_none());
        assert!(!ks.file_path().unwrap().exists());
    }

    #[test]
    fn corrupt_agent_secret_is_an_error() {
        let (_dir, ks) = temp_store();
        let real = Keypair::generate();
        let other = Keypair::generate();
        // Store `other`'s secret under `real`'s pubkey.
        let path = ks.file_path().unwrap();
        let blob = serde_json::json!({
            "agents": {
                real.public().to_string(): bs58::encode(other.secret_bytes().as_ref()).into_string(),
                other.public().to_string(): "not-base58-0OIl",
            }
        });
        write_atomic_0600(path, blob.to_string().as_bytes()).unwrap();
        assert!(matches!(ks.agent(&real.public()), Err(Error::Keystore(_))));
        assert!(matches!(ks.agent(&other.public()), Err(Error::Keystore(_))));
    }

    #[test]
    fn corrupt_json_is_an_error() {
        let (_dir, ks) = temp_store();
        write_atomic_0600(ks.file_path().unwrap(), b"{ not json").unwrap();
        assert!(matches!(ks.owner(), Err(Error::Json(_))));
    }

    #[test]
    fn empty_file_is_treated_as_empty_store() {
        let (_dir, ks) = temp_store();
        write_atomic_0600(ks.file_path().unwrap(), b"").unwrap();
        assert!(ks.list_agents().unwrap().is_empty());
        ks.owner().unwrap();
    }

    #[test]
    fn default_file_path_is_under_home() {
        // Only valid when AXEL_HOME is not set by the environment running
        // the tests; the env-driven path is covered by `open_honours_env`.
        if std::env::var_os(HOME_ENV).is_none() {
            let p = Keystore::default_file_path();
            assert!(p.ends_with(Path::new(".axel").join("secrets.json")));
        }
    }

    /// `AXEL_KEYSTORE=file` + `AXEL_HOME` select the file backend at the given
    /// directory. Never selects the keychain, so safe under `cargo test`.
    ///
    /// Runs in a single test (env vars are process-global) and uses
    /// `unsafe` set/remove as required by edition 2024.
    #[test]
    fn open_honours_env() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: tests in this crate that touch these variables are confined
        // to this function, and the variables are restored before returning.
        unsafe {
            std::env::set_var(HOME_ENV, dir.path());
            std::env::set_var(BACKEND_ENV, "file");
        }
        let result = (|| {
            let ks = Keystore::open()?;
            assert_eq!(ks.backend_name(), "file");
            assert_eq!(ks.file_path().unwrap(), dir.path().join("secrets.json"));
            ks.owner()?;
            assert!(dir.path().join("secrets.json").exists());

            // Case/whitespace tolerant.
            unsafe { std::env::set_var(BACKEND_ENV, " FILE ") };
            assert_eq!(Keystore::open()?.backend_name(), "file");

            // Unknown value is an error, not a silent keychain probe.
            unsafe { std::env::set_var(BACKEND_ENV, "cloud") };
            assert!(matches!(Keystore::open(), Err(Error::InvalidInput(_))));
            Ok::<(), Error>(())
        })();
        unsafe {
            std::env::remove_var(HOME_ENV);
            std::env::remove_var(BACKEND_ENV);
        }
        result.unwrap();
    }

    #[test]
    fn fromstr_error_is_std_error_send_sync() {
        fn assert_err<E: std::error::Error + Send + Sync + 'static>() {}
        assert_err::<<PublicKey as std::str::FromStr>::Err>();
        // And the anyhow-style call site works.
        let (_dir, ks) = temp_store();
        let kp = Keypair::generate();
        ks.put_agent(&kp).unwrap();
        let s = kp.public().to_string();
        let found = ks.agent(&s.parse().unwrap()).unwrap();
        assert!(found.is_some());
    }
}
