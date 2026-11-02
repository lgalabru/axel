//! axel-config: `AXEL.md` frontmatter + body, agent profiles, skill discovery, harness
//! skill drivers, launch specs and git helpers. See `docs/DESIGN.md` and
//! `docs/CONTRACTS.md`.
//!
//! ```
//! use axel_config::{AgentProfile, Harness, HarnessKind, WorkspaceConfig, launch};
//!
//! let config = WorkspaceConfig::parse(&WorkspaceConfig::template("demo")).unwrap();
//! assert_eq!(config.workspace.as_deref(), Some("demo"));
//!
//! let claude = AgentProfile::new("atlas", Harness::builtin(HarnessKind::Claude));
//! let spec = launch::build(&claude, Some("Add a README"));
//! assert_eq!(spec.program, "claude");
//! assert_eq!(spec.args.last().map(String::as_str), Some("Add a README"));
//! ```

mod config;
pub mod drivers;
mod error;
pub mod git;
pub mod launch;
pub mod remote;
mod skills;

pub use config::{
    AXEL_MD, AgentProfile, ApiConfig, GitHubSkills, Harness, HarnessKind, SkillSource, WorkspaceConfig,
    expand_tilde, is_base58_pubkey, split_frontmatter,
};
pub use error::Error;
pub use indexmap::IndexMap;
pub use skills::{
    SKILL_FILE, Skill, default_global_skills_dir, find_all_skills, find_skills, missing_skills, resolve_skills,
};
