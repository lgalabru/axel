# Cross-crate API contracts (P0)

Each crate is owned by one implementer. Other crates code against the signatures
below. If you must deviate, keep the name and change the body, not the shape.
All pubkeys cross crate boundaries as **base58 strings** (32-byte Ed25519 pubkeys);
only `axel-identity`/`axel-inbox` handle raw bytes.

## axel-identity

```rust
pub struct PublicKey(pub [u8; 32]);                 // Display/FromStr = base58
pub struct Keypair { /* ed25519_dalek::SigningKey, zeroized on drop */ }
impl Keypair {
    pub fn generate() -> Keypair;                   // getrandom::fill + SigningKey::from_bytes
    pub fn from_secret_bytes(b: &[u8; 32]) -> Keypair;
    pub fn public(&self) -> PublicKey;
    pub fn sign(&self, msg: &[u8]) -> [u8; 64];
    pub fn secret_bytes(&self) -> zeroize::Zeroizing<[u8; 32]>;
}
impl PublicKey { pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> Result<(), Error>; /* verify_strict */ }

// attestation (port of Buzz nip_oa.rs): domain "axel:agent-auth:v1:"
pub struct AuthTag { pub owner: PublicKey, pub conditions: String, pub sig: [u8; 64] }
pub fn attest(owner: &Keypair, agent: &PublicKey, conditions: &str) -> Result<AuthTag, Error>;
pub fn verify_attestation(tag: &AuthTag, agent: &PublicKey) -> Result<(), Error>;
impl AuthTag { pub fn to_tag(&self) -> Vec<String> /* ["auth", owner_b58, conditions, sig_hex] */; pub fn from_tag(&[String]) -> Result<Self, Error>; }

// keystore: keychain (service "axel", user "secrets", one JSON blob) with 0600 file fallback at ~/.axel/secrets.json
pub struct Keystore { /* backend */ }
impl Keystore {
    pub fn open() -> Result<Keystore, Error>;                       // default location
    pub fn open_at(fallback_file: std::path::PathBuf, use_keychain: bool) -> Result<Keystore, Error>;
    pub fn owner(&self) -> Result<Keypair, Error>;                  // creates on first call
    pub fn put_agent(&self, kp: &Keypair) -> Result<(), Error>;      // entry "agent:<b58>"; write then read-back verify
    pub fn agent(&self, pk: &PublicKey) -> Result<Option<Keypair>, Error>;
    pub fn remove_agent(&self, pk: &PublicKey) -> Result<(), Error>;
}
pub enum Error { ... }  // thiserror
```

## axel-inbox

```rust
pub mod kind { pub const NOTE: u32 = 1; pub const TASK: u32 = 2; pub const PERMISSION_REQUEST: u32 = 10;
  pub const PERMISSION_REPLY: u32 = 11; pub const QUESTION: u32 = 12; pub const ANSWER: u32 = 13;
  pub const STATUS: u32 = 20; pub const HANDOFF: u32 = 30; pub fn name(k: u32) -> &'static str; }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope { pub id: String /*hex sha256*/, pub from: String /*b58*/, pub created_at: u64,
                      pub kind: u32, pub tags: Vec<Vec<String>>, pub content: String, pub sig: String /*hex*/ }
pub struct EnvelopeBuilder { .. }
impl EnvelopeBuilder {
    pub fn new(kind: u32, content: impl Into<String>) -> Self;
    pub fn created_at(self, ts: u64) -> Self;           // default: now
    pub fn mention(self, pk_b58: &str) -> Self;         // ["p", pk]
    pub fn thread(self, thread_id: &str) -> Self;       // ["t", id]
    pub fn reply_to(self, root: &str, parent: &str) -> Self; // NIP-10 style ["e",root,"","root"],["e",parent,"","reply"]
    pub fn tag(self, tag: Vec<String>) -> Self;
    pub fn sign(self, kp: &axel_identity::Keypair) -> Result<Envelope, Error>;
}
impl Envelope {
    pub fn verify(&self) -> Result<(), Error>;          // recompute id, verify_strict sig over id bytes
    pub fn mentions(&self) -> Vec<&str>;                // p tags
    pub fn thread_id(&self) -> Option<&str>;            // first t tag
    pub fn root(&self) -> Option<&str>; pub fn parent(&self) -> Option<&str>;
}
pub fn canonical_bytes(from: &str, created_at: u64, kind: u32, tags: &[Vec<String>], content: &str) -> Vec<u8>;

// mentions (port of Buzz mentions.rs): "@atlas please review" -> ["p", atlas_pk]
pub struct Profile { pub pubkey: String, pub name: String }
pub fn resolve_mentions(text: &str, profiles: &[Profile]) -> Vec<String>;  // deduped pubkeys, cap 50, skips code spans
```

## axel-config

```rust
pub struct Harness { pub kind: HarnessKind, pub command: Option<String>, pub args: Vec<String> }
pub enum HarnessKind { Claude, Codex, Opencode, Custom }
pub struct AgentProfile { pub id: String, pub name: Option<String>, pub harness: Harness, pub model: Option<String>,
    pub skills: Vec<String>, pub prompt: Option<String>, pub allowed_tools: Vec<String>, pub disallowed_tools: Vec<String>,
    pub args: Vec<String>, pub color: Option<String>, pub pubkey: Option<String>, pub api: Option<ApiConfig> }
pub struct ApiConfig { pub protocol: Option<String>, pub provider: Option<String>, pub base_url: Option<String>, pub api_key_env: Option<String> }
pub enum SkillSource { Dir(PathBuf), GitHub(GitHubSkills) }   // skills.sh links, owner/repo/skill, github:owner/repo[@ref]
pub struct GitHubSkills { pub owner: String, pub repo: String, pub skill: Option<String>, pub git_ref: Option<String> }
pub struct WorkspaceConfig { pub workspace: Option<String>, pub skills: Vec<SkillSource>,
    pub body: String /* markdown after frontmatter */, pub path: Option<PathBuf> }   // no agents: they live in the store
impl WorkspaceConfig {
    pub fn load(dir: &Path) -> Result<Option<WorkspaceConfig>, Error>;   // reads dir/AXEL.md, None if absent
    pub fn parse(text: &str) -> Result<WorkspaceConfig, Error>;
    pub fn template(workspace_name: &str) -> String;
}
pub struct Skill { pub name: String, pub path: PathBuf, pub description: Option<String> }
pub fn find_all_skills(sources: &[SkillSource], global: Option<&Path>) -> Result<indexmap::IndexMap<String, Skill>, Error>;  // directories only
pub fn find_skills(roots: &[remote::SkillRoot], global: Option<&Path>) -> Result<indexmap::IndexMap<String, Skill>, Error>;  // dirs, then remote, then global
pub mod remote {   // AXEL.lock pins each remote source to a commit; cache <cache>/github/<owner>/<repo>/<commit>/
    pub struct Lock { pub skills: BTreeMap<String, String> }   // GitHubSkills::key() → commit
    pub enum SkillRoot { Dir(PathBuf), Files(Vec<PathBuf>) }
    pub fn resolve(config: &WorkspaceConfig, cache: &Path) -> Resolved;   // pins new sources, never moves a pin
    pub fn check_updates(config: &WorkspaceConfig, cache: &Path) -> (Lock, Vec<Update>, Vec<String>);
}
pub fn resolve_skills(requested: &[String], all: &IndexMap<String, Skill>) -> Vec<Skill>;  // "*" = all
pub mod drivers {
    pub trait SkillDriver { fn install(&self, project: &Path, skills: &[Skill]) -> Result<(), Error>; fn cleanup(&self, project: &Path) -> Result<(), Error>; fn install_context(&self, project: &Path, axel_md: &Path) -> Result<bool, Error> /* false: the project has its own context file */; }
    pub fn for_harness(kind: HarnessKind) -> Option<Box<dyn SkillDriver>>;
}
pub mod launch {
    pub struct LaunchSpec { pub program: String, pub args: Vec<String>, pub env: Vec<(String, String)> }
    pub fn build(profile: &AgentProfile, prompt: Option<&str>) -> LaunchSpec;  // claude: --model, --allowedTools...; codex: -c project_doc_fallback_filenames=...; custom: command
}
pub mod git { pub fn repo_root(dir: &Path) -> Option<PathBuf>; pub fn current_branch(dir: &Path) -> Option<String>;
    pub fn ensure_worktree(repo: &Path, branch: &str, dest: &Path) -> Result<PathBuf, Error>; pub fn list_worktrees(repo: &Path) -> Result<Vec<(PathBuf, String)>, Error>; }
```

## axel-store (event log + projections; depends on axel-identity and axel-inbox)

```rust
pub struct Store { conn: rusqlite::Connection }
impl Store {
    pub fn default_path() -> PathBuf;                       // $AXEL_DB, $AXEL_HOME/axel.db, ~/.axel/axel.db
    pub fn open(path: &Path) -> Result<Store, Error>; pub fn open_default() -> Result<Store, Error>; pub fn open_memory() -> Result<Store, Error>;

    // ---- the log ----
    pub fn append(&self, env: &Envelope) -> Result<Appended, Error>;   // verify sig; INSERT OR IGNORE into events; authorize; project. Idempotent.
    pub fn events_since(&self, cursor: i64, limit: usize) -> Result<Vec<(i64 /*seq*/, Envelope)>, Error>;   // for sync
    pub fn thread_events(&self, thread_id: &str) -> Result<Vec<Envelope>, Error>;                            // (created_at, id) order
    pub fn rebuild_projections(&self) -> Result<(), Error>;                                                   // replay the whole log

    // ---- typed writers: build + sign + append + project ----
    pub fn create_thread(&self, signer: &Keypair, title: &str, intent: &str, parent: Option<(&str, Relationship)>) -> Result<Thread, Error>;   // thread_id = envelope id; signer becomes owner participant
    pub fn update_thread(&self, signer: &Keypair, thread_id: &str, title: Option<&str>, state: Option<ThreadState>) -> Result<(), Error>;
    pub fn attach_project(&self, signer: &Keypair, thread_id: &str, repo_key: Option<&str>, path_hint: &Path, ws: &ThreadWorkspace) -> Result<(), Error>;
    pub fn detach_project(&self, signer: &Keypair, thread_id: &str, repo_key_or_path: &str) -> Result<(), Error>;
    pub fn add_participant(&self, signer: &Keypair, thread_id: &str, pubkey: &str, role: ParticipantRole) -> Result<(), Error>;
    pub fn start_session(&self, signer: &Keypair, s: &NewSession) -> Result<Session, Error>;    // session_id = new ULID; signer = the agent (or owner on its behalf)
    pub fn update_session(&self, signer: &Keypair, session_id: &str, u: &SessionUpdate) -> Result<(), Error>;
    pub fn end_session(&self, signer: &Keypair, session_id: &str, reason: Option<&str>) -> Result<(), Error>;
    pub fn record_status(&self, signer: &Keypair, session_id: &str, s: &StatusReport) -> Result<(), Error>;
    pub fn add_usage(&self, signer: &Keypair, session_id: &str, delta: &UsageDelta, transcript_offset: Option<u64>) -> Result<Usage, Error>;   // transcript_offset is LOCAL (sessions.transcript_offset), not in the envelope
    pub fn post(&self, env: &Envelope) -> Result<(), Error>;   // alias of append for notes/requests/replies built elsewhere

    // ---- projections: reads ----
    pub fn get_thread(&self, id: &str) -> Result<Option<Thread>, Error>;
    pub fn list_threads(&self, f: ThreadFilter) -> Result<Vec<Thread>, Error>;          // updated_at desc
    pub fn thread_summary(&self, id: &str) -> Result<Option<ThreadSummary>, Error>;
    pub fn thread_summaries(&self, f: ThreadFilter) -> Result<Vec<ThreadSummary>, Error>;
    pub fn thread_projects(&self, thread_id: &str) -> Result<Vec<ThreadProject>, Error>;
    pub fn participants(&self, thread_id: &str) -> Result<Vec<Participant>, Error>;
    pub fn get_session(&self, id: &str) -> Result<Option<Session>, Error>;
    pub fn list_sessions(&self, f: SessionFilter) -> Result<Vec<Session>, Error>;       // started_at desc
    pub fn latest_session_for_agent(&self, pubkey: &str) -> Result<Option<Session>, Error>;
    pub fn status(&self, session_id: &str) -> Result<Option<Status>, Error>; pub fn statuses(&self) -> Result<Vec<Status>, Error>;   // non-ended sessions only
    pub fn usage(&self, session_id: &str) -> Result<Option<Usage>, Error>; pub fn usages(&self) -> Result<Vec<Usage>, Error>;
    pub fn thread_usage(&self, thread_id: &str) -> Result<Usage, Error>;                 // sum over sessions; session_id = ""
    pub fn usage_samples(&self, session_id: Option<&str>, since: i64) -> Result<Vec<UsageSample>, Error>;
    pub fn thread_usage_samples(&self, thread_id: &str, since: i64) -> Result<Vec<UsageSample>, Error>;
    // inbox (unchanged semantics, over the events table)
    pub fn inbox(&self, pubkey: &str, limit: usize) -> Result<Vec<Envelope>, Error>;
    pub fn unread_count(&self, pubkey: &str) -> Result<u64, Error>;
    pub fn mark_read(&self, pubkey: &str, through_created_at: u64, through_id: &str) -> Result<(), Error>;
    pub fn pending_requests(&self) -> Result<Vec<Envelope>, Error>;
    pub fn replies_to(&self, id: &str) -> Result<Vec<Envelope>, Error>;
    pub fn thread_messages(&self, thread_id: &str) -> Result<Vec<Envelope>, Error>;      // kinds < 100 only

    // ---- local-only ----
    pub fn local_environment(&self) -> Result<Environment, Error>; pub fn set_environment_rex_host(&self, id: &str, host: &str) -> Result<(), Error>;
    pub fn upsert_project(&self, env_id: &str, path: &Path, repo_key: Option<&str>) -> Result<Project, Error>;
    pub fn recent_projects(&self, limit: usize) -> Result<Vec<Project>, Error>; pub fn get_project(&self, id: &str) -> Result<Option<Project>, Error>;
    pub fn project_by_repo_key(&self, env_id: &str, key: &str) -> Result<Option<Project>, Error>;
    pub fn insert_agent(&self, a: &NewAgent) -> Result<Agent, Error>; pub fn list_agents(&self) -> Result<Vec<Agent>, Error>;
    pub fn agent_by_name(&self, n: &str) -> Result<Option<Agent>, Error>; pub fn agent_by_pubkey(&self, pk: &str) -> Result<Option<Agent>, Error>;
    pub fn touch_agent(&self, pk: &str) -> Result<(), Error>; pub fn archive_agent(&self, pk: &str) -> Result<(), Error>;
    pub fn touch_agent(&self, pk: &str) -> Result<(), Error>; pub fn rename_agent(&self, pk: &str, name: &str) -> Result<(), Error>;   // key, policy, history stay; running sessions keep the old name
    pub fn touch_agent(&self, pk: &str) -> Result<(), Error>; pub fn set_agent_profile(&self, pk: &str, skills: Option<&[String]>, prompt: Option<&str>, color: Option<AgentColor>) -> Result<(), Error>;   // each Some replaces; "" clears the prompt
    pub fn known_identity_name(&self, pubkey: &str) -> Result<Option<String>, Error>;   // agent name, or participant display if known
}

pub struct Appended { pub inserted: bool, pub projected: bool, pub rejected_reason: Option<String> }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all="snake_case")] pub enum ThreadState { Open, Done, Archived }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all="snake_case")] pub enum SessionState { Starting, Running, Blocked, Idle, Ended }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all="snake_case")] pub enum WorkspaceKind { InPlace, Worktree, Scratch }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all="snake_case")] pub enum ParticipantRole { Owner, Member, Agent }
// Every closed set is an enum.  All of the above and below derive Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize
// (snake_case), and have `as_str()`, `FromStr` (Err = Error::Corrupt), `Display` (honours padding) and `const ALL`.
// DB columns stay TEXT and convert at the boundary; text a build does not know reads as the default (or None), never as an error.
#[derive(Default)] pub enum RuntimeMode { #[default] Autopilot, Edits, Supervised, Plan }
#[derive(Default)] pub enum StatusState { #[default] Idle, Working, Done, Blocked, Error }   // OSC 7501 states minus `clear`; `urgency()`: blocked > error > working > done > idle
pub enum BlockedKind { Permission, Question, Auth }
pub enum Relationship { Fork, Subagent, Delegation }
pub enum AgentColor { Purple, Yellow, Red, Green, Blue, Gray, Orange }                      // the TUI's seven agent colours, in cycle order
// Not Copy and no ALL: parsing never fails (anything that is not a builtin is Custom).  Serialised as the builtin name or the custom command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)] pub enum Harness { #[default] Claude, Codex, Opencode, Custom(String) }
impl Harness { pub const BUILTIN: [Harness; 3]; pub fn as_str(&self) -> &str; pub fn parse(s: &str) -> Harness; pub fn is_builtin(&self) -> bool; }   // FromStr<Err = Infallible>, Display, Serialize/Deserialize as a string
pub struct Thread { pub id: String, pub title: String, pub intent: String, pub state: ThreadState, pub created_by: String, pub parent_thread_id: Option<String>, pub relationship: Option<Relationship>, pub created_at: i64, pub updated_at: i64, pub done_at: Option<i64> }
pub struct ThreadWorkspace { pub kind: WorkspaceKind, pub branch: Option<String>, pub worktree_path: Option<PathBuf> }
pub struct ThreadProject { pub thread_id: String, pub repo_key: Option<String>, pub path_hint: PathBuf, pub workspace: ThreadWorkspace, pub local_project_id: Option<String> /* resolved on this replica by repo_key or path */, pub added_at: i64 }
pub struct Participant { pub thread_id: String, pub pubkey: String, pub role: ParticipantRole, pub added_by: String, pub added_at: i64 }
pub struct Session { pub id: String, pub thread_id: String, pub agent_pubkey: String, pub environment_id: String, pub environment_label: String, pub project_repo_key: Option<String>, pub harness: Harness, pub runtime_mode: RuntimeMode, pub harness_session_id: Option<String>, pub transcript_path: Option<String>, pub transcript_offset: i64, pub resumed_from: Option<String>, pub rex_session_id: Option<String>, pub rex_window_id: Option<String>, pub rex_block_id: Option<String>, pub state: SessionState, pub started_at: i64, pub ended_at: Option<i64>, pub ended_reason: Option<String> }
pub struct NewSession { pub thread_id: String, pub agent_pubkey: String, pub environment_id: String, pub environment_label: String, pub project_repo_key: Option<String>, pub harness: Harness, pub runtime_mode: RuntimeMode, pub resumed_from: Option<String> }
#[derive(Default)] pub struct SessionUpdate { pub state: Option<SessionState>, pub harness_session_id: Option<String>, pub transcript_path: Option<String>, pub rex: Option<(Option<String>, Option<String>, Option<String>)> }
pub struct StatusReport { pub state: StatusState, pub kind: Option<BlockedKind>, pub title: Option<String>, pub msg: Option<String>, pub progress: Option<u8> }   // kind-20 JSON: same snake_case strings as before; unknown text reads as Idle / None
pub struct Status { pub session_id: String, pub thread_id: String, pub state: StatusState, pub kind: Option<BlockedKind>, pub title: Option<String>, pub msg: Option<String>, pub progress: Option<u8>, pub updated_at: i64 }
pub struct Usage { pub session_id: String, pub input_tokens: i64, pub output_tokens: i64, pub cache_read_tokens: i64, pub cache_write_tokens: i64, pub turns: i64, pub cost_usd_est: f64, pub updated_at: i64 }
#[derive(Default)] pub struct UsageDelta { .. as before .. }
pub struct UsageSample { pub session_id: String, pub at: i64, pub input_tokens: i64, pub output_tokens: i64 }
pub struct ThreadSummary { pub thread: Thread, pub projects: Vec<ThreadProject>, pub participants: Vec<Participant>, pub sessions_total: i64, pub sessions_live: Vec<Session> /* non-ended */, pub agents: Vec<String>, pub environments: Vec<String /*labels*/>, pub usage: Usage, pub live: Option<Status> /* most urgent: blocked > error > working > done > idle */ }
#[derive(Default)] pub struct ThreadFilter { pub states: Option<Vec<ThreadState>>, pub agent_pubkey: Option<String>, pub participant: Option<String>, pub repo_key: Option<String>, pub limit: Option<usize> }
#[derive(Default)] pub struct SessionFilter { pub thread_id: Option<String>, pub agent_pubkey: Option<String>, pub environment_id: Option<String>, pub states: Option<Vec<SessionState>>, pub limit: Option<usize> }
pub struct Agent { pub pubkey: String, pub name: String, pub harness: Harness, pub model: Option<String>, pub skills: Vec<String>, pub prompt: Option<String>, pub color: Option<AgentColor>, pub runtime_mode: RuntimeMode, pub auth_tag: Option<Vec<String>>, pub created_at: i64, pub last_seen_at: Option<i64>, pub archived_at: Option<i64> }
#[derive(Default)] pub struct NewAgent { pub pubkey: String, pub name: String, pub harness: Harness, pub model: Option<String>, pub skills: Vec<String>, pub prompt: Option<String>, pub color: Option<AgentColor>, pub runtime_mode: RuntimeMode, pub auth_tag: Option<Vec<String>> }
// Environment and Project(+repo_key) unchanged.
```

Authorization in `append`: kind 100 → always (signer becomes owner). Other thread kinds → signer is a participant of `t`, or an agent whose `agents.auth_tag` owner is a participant, or (kinds 20/21/110–112) signer is one of this replica's own agents. Non-thread kinds (notes etc. without `t`) → always. Rejected envelopes are kept with `rejected_reason` and re-tried by `rebuild_projections`.

## axel-rex (shells out to the `rex` CLI with `--json`; no WS yet)

```rust
pub struct RexEnv { pub server: Option<String>, pub session: String, pub block: String }
pub fn detect() -> Option<RexEnv>;                      // from REX_SESSION/REX_BLOCK
pub struct Rex { env: RexEnv }
impl Rex {
    pub fn new(env: RexEnv) -> Rex;
    pub fn api_call(&self, method: &str, args: serde_json::Value) -> Result<serde_json::Value, Error>;   // `rex api call -s SESSION METHOD JSON`
    pub fn new_floating_terminal(&self, label: &str, bounds: [f64; 4], command: &[String], cwd: Option<&Path>, env: &[(String, String)]) -> Result<Layer, Error>;  // session.new_layer, shell "none"; env passed by wrapping with `/usr/bin/env K=V ... cmd`
    pub fn close_layer(&self, layer_id: &str) -> Result<(), Error>;
    pub fn new_window(&self, label: &str, command: &[String], cwd: Option<&Path>, env: &[(String, String)], focus: bool) -> Result<Window, Error>;  // session.new_window
    pub fn set_block_label(&self, block_id: &str, label: &str) -> Result<(), Error>;
    pub fn set_window_label(&self, window_id: &str, label: &str) -> Result<(), Error>;
    pub fn focus_window(&self, window_id: &str) -> Result<(), Error>;
    pub fn view(&self) -> Result<serde_json::Value, Error>;
    pub fn program_status(&self, block_id: &str) -> Result<serde_json::Value, Error>;
    pub fn host_id(&self) -> Result<String, Error>;    // server.status
}
pub struct Layer { pub layer_id: String, pub block_ids: Vec<String> }
pub struct Window { pub window_id: String, pub block_ids: Vec<String> }
pub mod osc {   // OSC 7501 program status, rev 0.4
    pub enum State { Idle, Working, Done, Blocked, Error, Clear }
    pub enum Kind { Permission, Question, Auth }
    pub struct Report<'a> { pub state: State, pub id: Option<&'a str>, pub kind: Option<Kind>, pub progress: Option<u8>, pub app: Option<&'a str>, pub title: Option<&'a str>, pub msg: Option<&'a str> }
    pub fn encode(r: &Report) -> String;   // ESC ] 7501 ; k=v:k=v ESC \ ; title/msg base64, length caps enforced
    pub fn emit_to_tty(r: &Report) -> std::io::Result<()>;   // writes to /dev/tty, ignores failure if no tty
}
```

## axel-tui

```rust
pub enum Screen { Dashboard, Launcher, Inbox }
pub struct LaunchRequest { pub title: String, pub projects: Vec<PathBuf> /* 0..n */, pub agent_pubkey: String, pub workspace: axel_store::WorkspaceKind, pub branch: Option<String>, pub thread_id: Option<String> /* Some = spawn onto an existing thread */ }
pub enum Outcome { Launch(LaunchRequest), FocusSession(String /*session_id*/), Quit }
pub struct Deps<'a> { pub store: &'a axel_store::Store, pub keystore: &'a axel_identity::Keystore, pub cwd: PathBuf, pub harnesses: Vec<&'static str> /* HarnessKind::BUILTIN names; the form converts the pick with axel_store::Harness::parse */ }
// model rows (AgentRow, DashAgentRow, ThreadRow, StatusRow, NewAgentSpec) carry axel_store's Harness / RuntimeMode / AgentColor / StatusState, never their strings;
// theme::agent_color(Option<AgentColor>), theme::state_color(StatusState) and model::state_glyph(StatusState) match on the enums.
pub fn run(deps: Deps, initial: Screen) -> anyhow::Result<Outcome>;   // owns the terminal (raw mode + alt screen), restores on exit/panic
pub type RedirectInputFn<'a> = dyn Fn(&str) -> anyhow::Result<()> + 'a;
pub fn run_stream(store: &axel_store::Store, owner_pk: &str, thread_id: &str, redirect_input: Option<&RedirectInputFn<'_>>) -> anyhow::Result<()>;
// With redirect_input, the stream reads plain typing and bracketed paste without
// echo or an alternate screen. The callback inserts it into the matching composer
// and focuses that pane. Navigation keys stay inert; Ctrl+C exits the stream.
// Dashboard screen (default): bashtop-style widgets — agents table (state glyph, name, harness, project, environment, tokens, sparkline from usage_samples, last event age), environments, tokens (1h series + totals + est cost), inbox panel. Enter on an agent → Outcome::FocusThread(thread_id) for its latest running thread; n → Launcher; i → Inbox; q → Quit. Refresh every 1s.
```
The TUI creates new agents itself: `Keypair::generate()` → `keystore.put_agent` → `attest(&keystore.owner()?, &pk, "")` → `store.insert_agent`.
Inbox screen: list pending requests + recent messages for the owner and all agents; `a`/`d` on a permission request writes a PERMISSION_REPLY envelope signed by the owner (content "allow"/"deny", reply_to(request.id, request.id), thread tag) via `axel_inbox::EnvelopeBuilder` and `store.insert_message`.

## axel-acp (managed runtime: ACP client, session host, policy)

The host prefers a model selector from ACP `configOptions` and sends
`session/set_config_option`; adapters without one retain `session/set_model`.
Standard `usage_update.used` is a context gauge, not billable token usage.
Its cumulative USD cost is recorded once, without adding it again from Axel's
usage extension or the prompt reply. Other currencies are not treated as USD.

```rust
// Adapter resolution (overridable by env AXEL_ACP_<HARNESS>=<command line>)
pub fn adapter_command(harness: &axel_store::Harness) -> Vec<String>;  // claude → [axel, "claude-acp"], codex → [axel, "codex-acp"], custom "goose" → ["goose","acp"], Custom(cmd) → split(cmd)

pub struct HostConfig { pub session_id: String, pub thread_id: String, pub agent_pubkey: String, pub cwd: PathBuf, pub harness: Harness, pub runtime_mode: RuntimeMode, pub disallowed_tools: Vec<String>, pub log_path: PathBuf, pub socket_path: PathBuf, pub env: Vec<(String,String)>, pub initial_prompt: Option<String>, pub resume_native_id: Option<String> }
pub async fn run_host(cfg: HostConfig, store: axel_store::Store, signer: axel_identity::Keypair, out: impl std::io::Write + Send) -> anyhow::Result<()>;
//   - spawns adapter, initialize (clientCapabilities fs read/write = true, terminal = false), session/new {cwd, mcpServers: []} (or session/load when resume_native_id and agent supports loadSession)
//   - stores native session id via store.update_session(harness_session_id)
//   - serves socket commands; `prompt` sends session/prompt and streams updates into store items; `cancel` → session/cancel
//   - on every session/update: push/update ConversationItem; on prompt completion: store.record_turn(..)
//   - session/request_permission → policy::decide(mode, tool_call, disallowed, guard) → Allow(option)/Deny(option)/Ask → Ask = Pending Permission item; wait for socket `answer`
//   - fs/read_text_file and fs/write_text_file client methods implemented against cwd (write respects plan mode: deny)
//   - prints one coloured line per event to `out`; appends {ts, dir, frame} JSON lines to log_path
//   - exits when socket `stop`, adapter exits, or stdin closes; then store.end_session(reason)

pub mod socket {  // JSON lines over a unix socket
    pub enum Command { Prompt { text: String, attachments, steer: bool, from: Option<Sender> }, Note { .. }, Steer { item_id }, Dequeue { item_id }, Edit { item_id, text }, SetMode { mode }, Cancel, Answer { item_id: String, option_id: String }, Status, Stop }
    pub enum Reply { Ok, Queued { item_id }, Steering { item_id }, Status { state: axel_store::SessionState, turn: i64, pending: usize, queued: usize }, Error { message: String } }
    // Prompt mid-turn: the owner's goes into the turn (`Steering`, pending until read; see
    // DESIGN "A message sent while the agent works"), another agent's waits (`Queued`).
    // Edit / Dequeue work on either until the agent reads it; `steer: true` interrupts.
    pub fn send(socket_path: &Path, cmd: &Command) -> std::io::Result<Reply>;   // sync, for the TUI/CLI
}
pub mod policy {
    pub enum Decision { Allow, Deny { reason: String }, Ask }
    pub fn decide(mode: RuntimeMode, tool_kind: ToolKind, title: &str, raw_input: &serde_json::Value, disallowed_tools: &[String]) -> Decision;
    pub enum ToolKind { Read, Edit, Delete, Move, Search, Execute, Think, Fetch, SwitchMode, Other }  // ACP tool kinds
    pub fn shell_deny_reason(cmd: &str) -> Option<String>;   // moved here from the bin's hook guard (bin re-exports it)
}
```

```rust
pub mod checkpoint {   // turn checkpoints: hidden commits of the tree before and after a turn
    pub const REFS: &str = "refs/axel/checkpoints";   // …/<thread>/<session>/<ordinal>/{before,after}
    pub const BUDGET: Duration;   // 10 s: the most a checkpoint may hold up a turn
    pub struct Snapshot { pub commit: String, pub tree: String }
    pub enum Which { Before, After }
    pub fn ref_name(thread_id: &str, session_id: &str, ordinal: i64, which: Which) -> String;
    pub fn capture(dir: &Path, ref_name: &str, budget: Duration) -> Result<Option<Snapshot>>;   // None: not a repo / no HEAD
    pub fn snapshot_commit(dir: &Path, message: &str) -> Result<String>;   // the remote handoff's snapshot (moved here)
    pub fn changes(dir: &Path, before: &Snapshot, after: &Snapshot, authored: &[PathBuf]) -> Result<TurnChanges>;
}
// The host snapshots in start_turn (before the prompt) and end_turn (after); the TurnEnd item and the
// turn envelope carry `changes`.  The Claude pane hooks (`axel hook claude`) do the same from
// UserPromptSubmit / PostToolUse / Stop, keeping the open turn in `TurnMark`.
```

## axel-store additions (managed runtime)

```rust
#[derive(Clone, Copy, ...)] #[serde(rename_all="snake_case")] pub enum SessionRuntime { Pane, Managed }   // Session.runtime, NewSession.runtime; wire session.start gains "runtime" (lenient default Pane)
// local-only conversation items (never in the shared log)
#[derive(Clone, Copy, ...)] pub enum ItemKind { UserMessage, AssistantMessage, Thought, ToolCall, Permission, Question, Answer, Error, TurnEnd }
#[derive(Clone, Copy, ...)] pub enum ItemStatus { Pending, Running, Completed, Failed, Cancelled, Allowed, Denied, Answered }
pub struct ConversationItem { pub id: String, pub session_id: String, pub thread_id: String, pub seq: i64 /* per session */, pub turn: i64, pub at: i64 /* unix ms */, pub kind: ItemKind, pub status: Option<ItemStatus>, pub ref_id: Option<String> /* ACP toolCallId / permission id */, pub content: serde_json::Value }
pub struct NewItem { pub session_id: String, pub thread_id: String, pub turn: i64, pub kind: ItemKind, pub status: Option<ItemStatus>, pub ref_id: Option<String>, pub content: serde_json::Value }
impl Store {
    pub fn push_item(&self, item: &NewItem) -> Result<ConversationItem, Error>;
    pub fn update_item(&self, id: &str, status: Option<ItemStatus>, content_patch: Option<serde_json::Value>) -> Result<(), Error>;  // patch = shallow JSON merge
    pub fn item_by_ref(&self, session_id: &str, ref_id: &str) -> Result<Option<ConversationItem>, Error>;
    pub fn items(&self, session_id: &str, after_seq: i64, limit: usize) -> Result<Vec<ConversationItem>, Error>;
    pub fn thread_items(&self, thread_id: &str, limit: usize) -> Result<Vec<ConversationItem>, Error>;   // all sessions, by (at, seq), newest last
    pub fn pending_items(&self, thread_id: Option<&str>) -> Result<Vec<ConversationItem>, Error>;   // Permission|Question with Pending
    pub fn set_session_socket(&self, session_id: &str, socket_path: Option<&Path>) -> Result<(), Error>;  // local column sessions.socket_path
    // shared turn summaries: envelope kind 40 "turn", tags t + s, content JSON = TurnSummary
    pub fn record_turn(&self, signer: &Keypair, session_id: &str, t: &TurnSummary) -> Result<(), Error>;
    pub fn turns(&self, thread_id: &str) -> Result<Vec<Turn>, Error>;
}
pub struct TurnSummary { pub ordinal: i64, pub user_text: String, pub assistant_text: String /* head ≤ 2000 chars */, pub tool_calls: i64, pub files_edited: i64, pub duration_ms: i64, pub stop_reason: String, pub changes: Option<TurnChanges> /* serde default, omitted when None */ }
// turn checkpoints: the shareable part (the diff stays in the repository that ran the turn)
pub struct TurnChanges { pub before: String /* commit */, pub after: String /* == before when nothing changed */, pub files: Vec<FileChange> /* ≤ MAX_FILES = 200 */, pub truncated: bool, pub between_turns: bool }
impl TurnChanges { pub const MAX_FILES: usize; pub fn totals(&self) -> (i64 /*added*/, i64 /*removed*/); pub fn authored(&self) -> usize }
pub struct FileChange { pub path: String, pub old_path: Option<String>, pub status: ChangeStatus, pub added: Option<i64> /* None: binary */, pub removed: Option<i64>, pub authored: bool }
#[serde(rename_all="snake_case")] pub enum ChangeStatus { Added, Modified, Deleted, Renamed }   // letter() / from_letter()
// local-only: where a pane session's turn stands between hook runs (table turn_marks)
pub struct TurnMark { pub session_id: String, pub ordinal: i64, pub open: bool, pub before_commit: Option<String>, pub before_tree: Option<String>, pub last_after_tree: Option<String>, pub user_text: String, pub authored: Vec<String>, pub tool_calls: i64, pub started_at: i64 /* unix ms */ }
impl Store { pub fn turn_mark(&self, session_id: &str) -> Result<Option<TurnMark>, Error>; pub fn put_turn_mark(&self, m: &TurnMark) -> Result<(), Error>; }
pub struct Turn { pub id: String, pub thread_id: String, pub session_id: String, pub from_pubkey: String, pub created_at: u64, pub summary: TurnSummary }
// Session gains: pub runtime: SessionRuntime, pub socket_path: Option<PathBuf>
```
`axel_inbox::kind::TURN = 40` ("turn"); `is_message(k)` stays `k < 20`.

## axel-tui additions (thread view)

```rust
// Deps gains: pub open_terminal: Option<&'a OpenThreadFn<'a>>;
// ThreadAction gains: OpenTerminal; Ctrl+T opens a floating shell in the thread's
// workspace. Ctrl+D returns Manage and opens the thread controls.
// manage::Action::StopAgent (`t`, "Agents") opens per-agent controls.
// manage::Outcome::StopAgent { agent: String } stops only that agent's live
// sessions on this thread. Existing Stop (`x`, "Stop all") remains thread-wide.
// Additional outcomes: PauseAgent { agent }, ResumeAgent { agent }, RemoveAgent { agent }.
// Host Request gains PauseSession { session_id: String, paused: bool }; the host
// signals the session locally and sync carries its lifecycle update.
// Step::AgentActions { agent: String, idx: usize } offers Pause/Resume, Kill,
// and Remove. Removal preserves history and registration; an explicit summon
// starts a new session and rejoins the roster.
// SessionState gains Paused ("paused"). Pause freezes a local session host and
// its descendant processes; Resume restores the previous state in place.
// Store::thread_agents(thread_id: &str) -> Result<Vec<String>, Error> derives
// the roster from session starts and applied participant add/remove events.
pub enum ItemBody { …, Changes { files: usize, authored: usize, added: i64, removed: i64, between_turns: bool } }   // a TurnEnd that changed files
pub enum Row { …, Changes(usize) }   // the change line under a turn: "↗ 3 files  +42 −7"
pub struct util::DiffLink { pub thread: String, pub session_id: String, pub turn: i64, pub rex_session: String, pub server: Option<String> }   // axel-links://diff?…
pub mod diff {   // the diff pane (pure state + a loop), run by `axel thread diff --pane`
    // `run_diff` browses on the primary screen using native terminal scrollback.
    // `r` temporarily enters line review; returning preserves the printed diff.
    // Files form one continuous stream. `file` and `cursor` address source;
    // `scroll` addresses the shared stream, including headers and notes.
    pub struct DiffData { pub agent, pub turn: i64, pub title, pub headline, pub files: Vec<FileEntry>, pub patches: Vec<FilePatch>, pub syntax: BTreeMap<String, Vec<Vec<SyntaxSpan>>> /* path → patch line → spans; optional display-only data */, pub between_turns, pub truncated, pub note: Option<String>, pub offer_restore: bool, pub can_undo: bool }
    pub struct SyntaxSpan { pub text: String, pub tone: SyntaxTone }
    pub enum SyntaxTone { Plain, Muted, Keyword, String, Number, Function, Type, Bold, Italic }
    // Syntax comes from the immutable before/after checkpoint blobs, never the
    // current working tree. Missing or oversized source falls back to plain code.
    pub struct FileEntry { pub path, pub label, pub status: char, pub added: Option<i64>, pub removed: Option<i64>, pub authored: bool }
    pub struct FilePatch { pub path, pub lines: Vec<DiffLine> }   // from parse_patch(git diff --no-color)
    pub struct DiffLine { pub kind: LineKind /* Hunk|Context|Add|Del|Meta */, pub old: Option<usize>, pub new: Option<usize>, pub text }
    pub struct Draft { pub path, pub start, pub end, pub side: Side /* old|new */, pub text, pub note }   // serde; kept by the caller
    pub enum Outcome { Close, Send(String), Undo, Restore, Turn(i64) }
    pub fn parse_patch(diff: &str) -> Vec<FilePatch>;
    pub fn review_message(turn: i64, drafts: &[Draft]) -> String;   // "Review of your turn N:" + one ```review block per note
    pub fn run_diff(data: DiffData, drafts: Vec<Draft>) -> Result<(Outcome, Vec<Draft>)>;
}
```
CLI: `axel thread diff THREAD [--session S] [--turn N] [--stat] [--pane]` (the pane flag is what a DiffLink click runs);
`axel thread undo THREAD [--session S] [--turn N] [--restore] [-y]`; `axel gc`.
`axel thread refresh THREAD` relaunches that thread's open Rex views on the
installed binary without ending or restarting any agent session.
`axel update` runs `curl -fsSL https://axel.md/install | sh`, preserving its
output and failing when either side of the pipeline fails.
The composer checks the latest published version entirely on a background
worker. It suggests `axel update` for a newer major/minor, or a patch gap >10.
Checks share a six-hour cache; network failures never interrupt composing.
ThreadView::update_hint: Option<String> is display-only background update state.
The composer reserves one extra row for it, leaving drafts and status intact.

## axel (bin) additions: checkpoints

Question multi-selection: `ChoiceOption::exclusive: bool` marks a decline option.
`ItemBody::Choice` gains `multiple: Option<(usize, usize)>`, its selection bounds.
`ThreadView::choice_checked` retains checked option IDs by item ID. Multi-select
questions use Space/digits to toggle, Enter to confirm, and arrows to move.
Exclusive decline choices clear other selections. Single-choice behavior and
permission grants stay unchanged; failed sends preserve checked choices.
Native adapters set `rawInput.multiSelect`, with optional `minSelections` and
`maxSelections`, defaulting to one through all offered selections.
The host persists validated bounds as `multi_select: {min,max}` on Question items.
The existing Answer command carries `answer:multi:<JSON array of option IDs>`
only for such questions. The host validates bounds, uniqueness and offered
allow-options before returning that encoded ID through ACP to Axel's adapter.
Claude `AskUserQuestion` receives selected labels joined by comma and space;
MCP array enum elicitations receive the selected original values as an array.
Decline/cancel remain distinct from accepting an allowed empty array. Ordinary
permissions never accept the composite answer encoding. Existing live hosts
require a restart to emit and accept multi-select question cards.

`theme::TerminalColors { foreground: (u8,u8,u8), background: (u8,u8,u8) }`
and `theme::terminal_colors() -> Option<TerminalColors>` expose one cached,
150ms-bounded terminal palette query. Call before crossterm starts reading input.
Unsupported terminals return `None`; native tool-detail bands then reset to the
terminal foreground/background rather than assuming a dark surface. Supported
terminals derive subtle bands and inline added/removed backgrounds from their
actual background, for both light and dark themes.

Usage reconciliation: `NativeUsage { id: String, at: i64, delta: UsageDelta }`.
`Store::replace_native_usage(session_id, &[NativeUsage])` atomically replaces a
local, rebuildable native-log cache. `effective_usage_log` substitutes its token
and cost records for that session's signed usage reports, retaining reported turn
counts. `effective_usage` groups that view by session. All usage reads and budget
checks use these views. Neither signed envelopes nor the raw projections change.
Missing native files retain the previous cache; sessions without a cache use
their reported usage. Local Claude messages are deduplicated by request/message
ID; Codex cumulative counters are differenced once. Native timestamps and model
prices determine rolling windows. A native conversation resumed into multiple
Axel sessions assigns each call to the latest session started before it.
`axel usage --refresh` reconciles native logs and prints the current per-agent
24-hour estimates. The usage pane reconciles on a background worker every 30s.
`pricing::estimate_call(model, input, output, cache_read, cache_write, cache_write_1h)`
prices one API call, including supported Codex long-context tiers and Claude's
one-hour cache writes. `estimate` remains the standard-rate aggregate fallback.
`ClaudeMeter::resumed()` seeds the first cumulative cost baseline without
charging historical spend; only calls observed in this process are estimated.

```rust
mod undo {   // reverse a turn in the working tree; the index and HEAD are untouched
    pub fn files_of(c: &TurnChanges) -> Vec<&FileChange>;   // authored files, or all when none is marked
    pub fn blocked_reason(ctx: &Ctx, session_id: &str) -> Result<Option<String>>;   // Some while the agent is mid-turn
    pub fn undo(dir: &Path, thread_id: &str, c: &TurnChanges) -> Result<UndoResult>;   // Done(Undone { files, snapshot_ref }) | Conflicts(paths)
    pub fn restore(dir: &Path, thread_id: &str, c: &TurnChanges) -> Result<Undone>;   // files back to their `before` content
    pub fn notify(ctx, thread_id, session_id, turn, files, restored) -> Result<()>;   // an fyi note to the session's agent
}
mod gc {   // prune refs/axel/checkpoints: ARCHIVED_KEEP_SECS = 7 d, LIVE_KEEP_SECS = 30 d, KEEP_TURNS = 20
    pub fn prune(dir: &Path, now: i64, info: &dyn Fn(&str) -> ThreadInfo) -> Result<Vec<String>>;
    pub fn run(ctx: &Ctx, quiet: bool) -> Result<usize>;
}
host::fetch_checkpoints(host: &Environment, dir: &Path, thread_id: &str) -> Result<()>;   // git fetch +refs/axel/checkpoints/<thread>/* from the host's repo
```

```rust
pub enum Screen { Dashboard, Launcher, Inbox, Thread(String /*thread_id*/) }
pub enum Outcome { Launch(LaunchRequest), FocusSession(String), OpenLog(String /*session_id*/), Quit }
// Deps gains: pub send: &'a dyn Fn(&str /*session_id*/, axel_acp::socket::Command) -> anyhow::Result<axel_acp::socket::Reply>
//            pub summon: &'a dyn Fn(&str /*thread_id*/, &str /*agent name*/, &str /*text*/) -> anyhow::Result<String /*session_id*/>
```
Thread view: header (title · agents with state glyphs · env · tokens · $), timeline from `thread_items` + `turns` (turns used only when a session has no local items anywhere, not merely outside the latest 500-item window), folds for consecutive ToolCall items between prose, Space/→ expands a fold, Shift-Space folds all, `l` → `Outcome::OpenLog(session_id of item under cursor or latest live)`, pending Permission/Question rendered inline with options; ↑/↓ moves, Enter on a pending item picks the highlighted option → `send(Answer)`; composer at the bottom: Enter → `send(Prompt)` to the thread's latest live managed session, or `summon()` when text starts with `@name` (or when no live session exists: summon the thread's last agent); Esc → Dashboard. Dashboard: Enter on a thread → `Screen::Thread`. Poll every 300 ms while a session is live.
