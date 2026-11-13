//! The signed message envelope.

use std::time::{SystemTime, UNIX_EPOCH};

use axel_identity::{Keypair, PublicKey, hex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::threading::{parse_thread_markers, reply_tags};
use crate::{Error, MENTION_CAP, kind};

/// A signed message. NIP-01 shape, Ed25519 signature.
///
/// `id` is the lowercase hex SHA-256 of [`canonical_bytes`]; `sig` is the
/// lowercase hex Ed25519 signature over the 32 raw id bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    /// Hex SHA-256 of the canonical form.
    pub id: String,
    /// Sender public key, base58.
    pub from: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Message kind, see [`kind`].
    pub kind: u32,
    /// Tags: `["p", pk]`, `["t", thread]`, `["e", id, "", "root"|"reply"]`, …
    pub tags: Vec<Vec<String>>,
    /// Message body.
    pub content: String,
    /// Hex Ed25519 signature over the id bytes.
    pub sig: String,
}

/// The bytes that are hashed into an envelope id:
/// `[0, from, created_at, kind, tags, content]` as compact JSON.
pub fn canonical_bytes(
    from: &str,
    created_at: u64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
) -> Vec<u8> {
    // serde_json's compact writer never emits whitespace, and an array of
    // primitives/strings/arrays has no key-ordering ambiguity.
    serde_json::to_vec(&(0u8, from, created_at, kind, tags, content))
        .expect("serializing primitives and strings cannot fail")
}

/// Hex SHA-256 of the canonical form.
pub fn compute_id(
    from: &str,
    created_at: u64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    let digest = Sha256::digest(canonical_bytes(from, created_at, kind, tags, content));
    hex::encode(&digest)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Envelope {
    /// Recompute the id and verify the signature (strictly) over it.
    pub fn verify(&self) -> Result<(), Error> {
        let pk: PublicKey = self
            .from
            .parse()
            .map_err(|_| Error::InvalidPubkey(self.from.clone()))?;
        let expected = compute_id(
            &self.from,
            self.created_at,
            self.kind,
            &self.tags,
            &self.content,
        );
        if expected != self.id {
            return Err(Error::IdMismatch {
                expected,
                actual: self.id.clone(),
            });
        }
        let id_bytes = hex::decode_array::<32>(&self.id).ok_or_else(|| Error::IdMismatch {
            expected: expected.clone(),
            actual: self.id.clone(),
        })?;
        let sig = hex::decode_array::<64>(&self.sig).ok_or_else(|| {
            Error::InvalidSignature(format!("expected 128 hex chars, got {}", self.sig.len()))
        })?;
        pk.verify(&id_bytes, &sig)?;
        Ok(())
    }

    /// Human-readable kind name.
    pub fn kind_name(&self) -> &'static str {
        kind::name(self.kind)
    }

    /// Second element of every tag whose first element is `name`.
    pub fn tag_values<'a, 'n>(
        &'a self,
        name: &'n str,
    ) -> impl Iterator<Item = &'a str> + use<'a, 'n> {
        self.tags
            .iter()
            .filter(move |t| t.len() >= 2 && t[0] == name)
            .map(|t| t[1].as_str())
    }

    /// Second element of the first tag named `name`.
    pub fn first_tag(&self, name: &str) -> Option<&str> {
        self.tag_values(name).next()
    }

    /// Mentioned pubkeys (`p` tags), in tag order.
    pub fn mentions(&self) -> Vec<&str> {
        self.tag_values("p").collect()
    }

    /// The Axel thread this concerns (first `t` tag).
    pub fn thread_id(&self) -> Option<&str> {
        self.first_tag("t")
    }

    /// The harness session this concerns (first `s` tag).
    pub fn session_id(&self) -> Option<&str> {
        self.first_tag("s")
    }

    /// The `title` tag (first one), used by `thread.create`.
    pub fn title_tag(&self) -> Option<&str> {
        self.first_tag("title")
    }

    /// Root of the conversation this message belongs to.
    ///
    /// The `root` marker if present; otherwise the `reply` marker (a direct
    /// reply to the root names only its parent). `None` for a top-level
    /// message.
    pub fn root(&self) -> Option<&str> {
        let m = parse_thread_markers(&self.tags);
        let id = m.root.or(m.reply)?;
        self.tag_values("e").find(|v| *v == id)
    }

    /// The message this one replies to (`reply` marker). `None` if this is
    /// not a reply.
    pub fn parent(&self) -> Option<&str> {
        let m = parse_thread_markers(&self.tags);
        let id = m.reply?;
        self.tag_values("e").find(|v| *v == id)
    }

    /// True when this message replies to `id`.
    pub fn is_reply_to(&self, id: &str) -> bool {
        self.parent() == Some(id)
    }
}

/// Builds and signs an [`Envelope`].
#[derive(Clone, Debug)]
pub struct EnvelopeBuilder {
    kind: u32,
    content: String,
    created_at: Option<u64>,
    tags: Vec<Vec<String>>,
}

impl EnvelopeBuilder {
    /// Start an envelope of `kind` with `content`.
    pub fn new(kind: u32, content: impl Into<String>) -> Self {
        EnvelopeBuilder {
            kind,
            content: content.into(),
            created_at: None,
            tags: Vec::new(),
        }
    }

    /// Set the timestamp (unix seconds). Defaults to now at `sign` time.
    pub fn created_at(mut self, ts: u64) -> Self {
        self.created_at = Some(ts);
        self
    }

    /// Add a `["p", pk]` recipient. Duplicate pubkeys are ignored.
    pub fn mention(mut self, pk_b58: &str) -> Self {
        let dup = self
            .tags
            .iter()
            .any(|t| t.len() >= 2 && t[0] == "p" && t[1] == pk_b58);
        if !dup {
            self.tags.push(vec!["p".into(), pk_b58.into()]);
        }
        self
    }

    /// Add every pubkey as a mention.
    pub fn mentions<I, S>(mut self, pks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for pk in pks {
            self = self.mention(pk.as_ref());
        }
        self
    }

    /// Add a `["t", thread_id]` tag.
    pub fn thread(mut self, thread_id: &str) -> Self {
        self.tags.push(vec!["t".into(), thread_id.into()]);
        self
    }

    /// Add an `["s", session_id]` tag.
    pub fn session(mut self, session_id: &str) -> Self {
        self.tags.push(vec!["s".into(), session_id.into()]);
        self
    }

    /// Add a `["title", title]` tag.
    pub fn title(mut self, title: &str) -> Self {
        self.tags.push(vec!["title".into(), title.into()]);
        self
    }

    /// Mark this as a reply: `["e", root, "", "root"]` and
    /// `["e", parent, "", "reply"]`. For a direct reply to the root, pass the
    /// same id for both.
    pub fn reply_to(mut self, root: &str, parent: &str) -> Self {
        self.tags.extend(reply_tags(root, parent));
        self
    }

    /// Add an arbitrary tag.
    pub fn tag(mut self, tag: Vec<String>) -> Self {
        self.tags.push(tag);
        self
    }

    /// Compute the id, sign it with `kp`, and return the envelope.
    ///
    /// Fails with [`Error::TooManyMentions`] when more than
    /// [`MENTION_CAP`] `p` tags are present.
    pub fn sign(self, kp: &Keypair) -> Result<Envelope, Error> {
        let mention_count = self
            .tags
            .iter()
            .filter(|t| t.len() >= 2 && t[0] == "p")
            .count();
        if mention_count > MENTION_CAP {
            return Err(Error::TooManyMentions(mention_count));
        }
        let from = kp.public().to_base58();
        let created_at = self.created_at.unwrap_or_else(now);
        let id = compute_id(&from, created_at, self.kind, &self.tags, &self.content);
        let id_bytes = hex::decode_array::<32>(&id).expect("sha256 hex is 64 chars");
        let sig = hex::encode(&kp.sign(&id_bytes));
        Ok(Envelope {
            id,
            from,
            created_at,
            kind: self.kind,
            tags: self.tags,
            content: self.content,
            sig,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id64(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    #[test]
    fn canonical_bytes_are_compact_json_array() {
        let tags = vec![vec!["p".to_string(), "abc".to_string()]];
        let bytes = canonical_bytes("FROM", 42, kind::NOTE, &tags, "hi \"there\"\n");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"[0,"FROM",42,1,[["p","abc"]],"hi \"there\"\n"]"#
        );
        // Unicode is passed through, not escaped.
        let bytes = canonical_bytes("F", 0, 1, &[], "héllo ☃");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"[0,"F",0,1,[],"héllo ☃"]"#
        );
    }

    #[test]
    fn compute_id_is_sha256_hex() {
        let id = compute_id("F", 0, 1, &[], "");
        assert_eq!(id.len(), 64);
        assert!(hex::is_lowercase_hex(&id));
        // Known vector: sha256 of the exact canonical string.
        let expected = hex::encode(&Sha256::digest(br#"[0,"F",0,1,[],""]"#));
        assert_eq!(id, expected);
        // Deterministic.
        assert_eq!(id, compute_id("F", 0, 1, &[], ""));
        // Sensitive to every field.
        assert_ne!(id, compute_id("G", 0, 1, &[], ""));
        assert_ne!(id, compute_id("F", 1, 1, &[], ""));
        assert_ne!(id, compute_id("F", 0, 2, &[], ""));
        assert_ne!(
            id,
            compute_id("F", 0, 1, &[vec!["t".into(), "x".into()]], "")
        );
        assert_ne!(id, compute_id("F", 0, 1, &[], " "));
    }

    #[test]
    fn sign_verify_roundtrip() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::NOTE, "hello")
            .created_at(1_700_000_000)
            .mention("SomePk")
            .thread("thr_1")
            .sign(&kp)
            .unwrap();
        assert_eq!(env.from, kp.public().to_string());
        assert_eq!(env.created_at, 1_700_000_000);
        assert_eq!(env.kind, kind::NOTE);
        assert_eq!(env.kind_name(), "note");
        assert_eq!(env.content, "hello");
        assert_eq!(env.sig.len(), 128);
        assert_eq!(
            env.id,
            compute_id(&env.from, env.created_at, env.kind, &env.tags, &env.content)
        );
        env.verify().unwrap();
        // Signature is directly over the id bytes.
        let id_bytes = hex::decode_array::<32>(&env.id).unwrap();
        let sig = hex::decode_array::<64>(&env.sig).unwrap();
        kp.public().verify(&id_bytes, &sig).unwrap();
    }

    #[test]
    fn default_created_at_is_now() {
        let before = now();
        let env = EnvelopeBuilder::new(kind::NOTE, "x")
            .sign(&Keypair::generate())
            .unwrap();
        let after = now();
        assert!(env.created_at >= before && env.created_at <= after);
        assert!(env.created_at > 1_600_000_000);
    }

    #[test]
    fn tampered_content_fails() {
        let kp = Keypair::generate();
        let mut env = EnvelopeBuilder::new(kind::TASK, "do the thing")
            .sign(&kp)
            .unwrap();
        env.content.push('!');
        assert!(matches!(env.verify(), Err(Error::IdMismatch { .. })));
    }

    #[test]
    fn tampered_fields_fail() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::TASK, "do")
            .mention("A")
            .sign(&kp)
            .unwrap();

        let mut e = env.clone();
        e.kind = kind::NOTE;
        assert!(e.verify().is_err());

        let mut e = env.clone();
        e.created_at += 1;
        assert!(e.verify().is_err());

        let mut e = env.clone();
        e.tags.push(vec!["p".into(), "B".into()]);
        assert!(e.verify().is_err());

        let mut e = env.clone();
        e.tags.clear();
        assert!(e.verify().is_err());

        // Changing `from` to another valid key: id mismatches.
        let mut e = env.clone();
        e.from = Keypair::generate().public().to_string();
        assert!(matches!(e.verify(), Err(Error::IdMismatch { .. })));

        // Garbage `from`.
        let mut e = env.clone();
        e.from = "not-a-key".into();
        assert!(matches!(e.verify(), Err(Error::InvalidPubkey(_))));
    }

    #[test]
    fn recomputed_id_but_wrong_signature_fails() {
        let kp = Keypair::generate();
        let other = Keypair::generate();
        let mut env = EnvelopeBuilder::new(kind::NOTE, "x").sign(&kp).unwrap();
        // Re-sign the same id with another key: id matches, sig does not.
        let id_bytes = hex::decode_array::<32>(&env.id).unwrap();
        env.sig = hex::encode(&other.sign(&id_bytes));
        assert!(matches!(env.verify(), Err(Error::Identity(_))));

        // Flip one bit of the real signature.
        let mut env = EnvelopeBuilder::new(kind::NOTE, "x").sign(&kp).unwrap();
        let mut sig = hex::decode_array::<64>(&env.sig).unwrap();
        sig[10] ^= 0x04;
        env.sig = hex::encode(&sig);
        assert!(env.verify().is_err());

        // Malformed sig encodings.
        let mut env = EnvelopeBuilder::new(kind::NOTE, "x").sign(&kp).unwrap();
        env.sig = "abc".into();
        assert!(matches!(env.verify(), Err(Error::InvalidSignature(_))));
        env.sig = "zz".repeat(64);
        assert!(matches!(env.verify(), Err(Error::InvalidSignature(_))));
    }

    #[test]
    fn id_is_case_sensitive() {
        let kp = Keypair::generate();
        let mut env = EnvelopeBuilder::new(kind::NOTE, "x").sign(&kp).unwrap();
        env.id = env.id.to_uppercase();
        assert!(env.verify().is_err());
    }

    #[test]
    fn mention_dedupes_and_mentions_accessor() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::NOTE, "x")
            .mention("A")
            .mention("B")
            .mention("A")
            .mentions(["C", "B"])
            .sign(&kp)
            .unwrap();
        assert_eq!(env.mentions(), vec!["A", "B", "C"]);
    }

    #[test]
    fn too_many_mentions_rejected() {
        let kp = Keypair::generate();
        let mut b = EnvelopeBuilder::new(kind::NOTE, "x");
        for i in 0..MENTION_CAP {
            b = b.mention(&format!("pk{i}"));
        }
        assert!(b.clone().sign(&kp).is_ok());
        let err = b.mention("one-more").sign(&kp).unwrap_err();
        assert!(matches!(err, Error::TooManyMentions(n) if n == MENTION_CAP + 1));
    }

    #[test]
    fn thread_and_custom_tags() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::STATUS, "working")
            .thread("thr_1")
            .thread("thr_2")
            .tag(vec!["h".into(), "claude".into()])
            .tag(vec!["k".into(), "permission".into()])
            .sign(&kp)
            .unwrap();
        assert_eq!(env.thread_id(), Some("thr_1"));
        assert_eq!(env.first_tag("h"), Some("claude"));
        assert_eq!(env.first_tag("k"), Some("permission"));
        assert_eq!(env.first_tag("zzz"), None);
        assert_eq!(
            env.tag_values("t").collect::<Vec<_>>(),
            vec!["thr_1", "thr_2"]
        );
        env.verify().unwrap();
    }

    #[test]
    fn session_and_title_tags() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::THREAD_CREATE, "ship the inbox")
            .title("Inbox v0")
            .session("01JSESSION")
            .thread("thr")
            .sign(&kp)
            .unwrap();
        assert_eq!(env.title_tag(), Some("Inbox v0"));
        assert_eq!(env.session_id(), Some("01JSESSION"));
        assert_eq!(env.thread_id(), Some("thr"));
        assert_eq!(env.kind_name(), "thread.create");
        assert!(
            env.tags
                .contains(&vec!["s".to_string(), "01JSESSION".to_string()])
        );
        assert!(
            env.tags
                .contains(&vec!["title".to_string(), "Inbox v0".to_string()])
        );
        env.verify().unwrap();

        let plain = EnvelopeBuilder::new(kind::NOTE, "x").sign(&kp).unwrap();
        assert_eq!(plain.session_id(), None);
        assert_eq!(plain.title_tag(), None);
    }

    #[test]
    fn reply_to_sets_root_and_parent() {
        let kp = Keypair::generate();
        let root = id64('1');
        let parent = id64('2');
        let env = EnvelopeBuilder::new(kind::ANSWER, "42")
            .reply_to(&root, &parent)
            .sign(&kp)
            .unwrap();
        assert_eq!(env.root(), Some(root.as_str()));
        assert_eq!(env.parent(), Some(parent.as_str()));
        assert!(env.is_reply_to(&parent));
        assert!(!env.is_reply_to(&root));
        assert_eq!(
            env.tags,
            vec![
                vec!["e", &root, "", "root"],
                vec!["e", &parent, "", "reply"],
            ]
        );
    }

    #[test]
    fn direct_reply_to_root_uses_same_id_twice() {
        // The TUI pattern: reply_to(request.id, request.id).
        let kp = Keypair::generate();
        let request = EnvelopeBuilder::new(kind::PERMISSION_REQUEST, "rm -rf?")
            .thread("thr")
            .sign(&kp)
            .unwrap();
        let reply = EnvelopeBuilder::new(kind::PERMISSION_REPLY, "deny")
            .reply_to(&request.id, &request.id)
            .thread("thr")
            .sign(&kp)
            .unwrap();
        assert_eq!(reply.root(), Some(request.id.as_str()));
        assert_eq!(reply.parent(), Some(request.id.as_str()));
        assert!(reply.is_reply_to(&request.id));
        assert_eq!(request.root(), None);
        assert_eq!(request.parent(), None);
    }

    #[test]
    fn root_only_tag_has_root_but_no_parent() {
        let kp = Keypair::generate();
        let root = id64('a');
        let env = EnvelopeBuilder::new(kind::NOTE, "x")
            .tag(vec!["e".into(), root.clone(), "".into(), "root".into()])
            .sign(&kp)
            .unwrap();
        assert_eq!(env.root(), Some(root.as_str()));
        assert_eq!(env.parent(), None);
    }

    #[test]
    fn malformed_e_tags_are_not_thread_links() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::NOTE, "x")
            .tag(vec!["e".into(), "short".into(), "".into(), "reply".into()])
            .tag(vec!["e".into(), id64('b')])
            .sign(&kp)
            .unwrap();
        assert_eq!(env.root(), None);
        assert_eq!(env.parent(), None);
    }

    #[test]
    fn serde_roundtrip_preserves_verifiability() {
        let kp = Keypair::generate();
        let env = EnvelopeBuilder::new(kind::HANDOFF, "context: …")
            .mention("A")
            .thread("t")
            .reply_to(&id64('1'), &id64('2'))
            .sign(&kp)
            .unwrap();
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
        back.verify().unwrap();
        // Field names are the NIP-01 ones.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for k in ["id", "from", "created_at", "kind", "tags", "content", "sig"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
    }
}
