//! Local tables: environments, projects, and agents (profiles and their
//! instances).  Profiles are also projected from kind-122 envelopes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Row, params};

use crate::{Agent, AgentProfile, Environment, Error, Harness, NewAgent, Project, Store, new_id, now, sql_limit};

// ---------------------------------------------------------------------------
// environments
// ---------------------------------------------------------------------------

/// Best-effort hostname: `$HOSTNAME`, then `hostname(1)`, then `"local"`.
fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return h.trim().to_string();
    }
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    "local".to_string()
}

pub(crate) fn row_environment(r: &Row<'_>) -> rusqlite::Result<Environment> {
    Ok(Environment {
        id: r.get("id")?,
        label: r.get("label")?,
        rex_host_id: r.get("rex_host_id")?,
        is_local: r.get::<_, i64>("is_local")? != 0,
        created_at: r.get("created_at")?,
        ssh_target: r.get("ssh_target")?,
        host_pubkey: r.get("host_pubkey")?,
    })
}

pub(crate) const ENV_COLS: &str = "id, label, rex_host_id, is_local, created_at, ssh_target, host_pubkey";

impl Store {
    /// The environment row for this machine, created on first call with the
    /// hostname as label.  `rex_host_id` is filled in later by
    /// [`Store::set_environment_rex_host`] once the caller has asked Rex.
    pub fn local_environment(&self) -> Result<Environment, Error> {
        if let Some(env) = self
            .conn
            .query_row(
                &format!("SELECT {ENV_COLS} FROM environments WHERE is_local = 1"),
                [],
                row_environment,
            )
            .optional()?
        {
            return Ok(env);
        }
        let env = Environment {
            id: new_id(),
            label: hostname(),
            rex_host_id: None,
            is_local: true,
            created_at: now(),
            ssh_target: None,
            host_pubkey: None,
        };
        self.conn.execute(
            "INSERT INTO environments (id, label, rex_host_id, is_local, created_at) VALUES (?1, ?2, NULL, 1, ?3)",
            params![env.id, env.label, env.created_at],
        )?;
        Ok(env)
    }

    /// The local environment's id if one has been created, without creating it.
    pub(crate) fn local_environment_id(&self) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row("SELECT id FROM environments WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Every environment, local first, then by label.
    pub fn list_environments(&self) -> Result<Vec<Environment>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {ENV_COLS} FROM environments ORDER BY is_local DESC, label"
        ))?;
        let rows = st.query_map([], row_environment)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Record the Rex `host_id` an environment is reachable as.
    pub fn set_environment_rex_host(&self, id: &str, rex_host_id: &str) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE environments SET rex_host_id = ?2 WHERE id = ?1",
            params![id, rex_host_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("environment {id}")));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// projects
// ---------------------------------------------------------------------------

const PROJECT_COLS: &str =
    "id, environment_id, path, name, repo_key, axel_md_present, last_opened_at";

fn row_project(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get("id")?,
        environment_id: r.get("environment_id")?,
        path: PathBuf::from(r.get::<_, String>("path")?),
        name: r.get("name")?,
        repo_key: r.get("repo_key")?,
        has_axel_md: r.get::<_, i64>("axel_md_present")? != 0,
        last_opened_at: r.get("last_opened_at")?,
    })
}

/// Display name for a project directory: its last component, or the whole path for `/`.
fn project_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

impl Store {
    /// Insert or refresh a project keyed by `(environment_id, path)`.  Always
    /// bumps `last_opened_at` and re-checks whether `AXEL.md` exists; a
    /// `repo_key` of `None` keeps the stored one.
    pub fn upsert_project(
        &self,
        env_id: &str,
        path: &Path,
        repo_key: Option<&str>,
    ) -> Result<Project, Error> {
        let path_str = path.to_string_lossy().into_owned();
        let has_axel_md = path.join("AXEL.md").is_file();
        self.conn.execute(
            "INSERT INTO projects (id, environment_id, path, name, repo_key, axel_md_present, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (environment_id, path) DO UPDATE SET
                name = excluded.name,
                repo_key = COALESCE(excluded.repo_key, projects.repo_key),
                axel_md_present = excluded.axel_md_present,
                last_opened_at = excluded.last_opened_at",
            params![new_id(), env_id, path_str, project_name(path), repo_key, i64::from(has_axel_md), now()],
        )?;
        Ok(self.conn.query_row(
            &format!("SELECT {PROJECT_COLS} FROM projects WHERE environment_id = ?1 AND path = ?2"),
            params![env_id, path_str],
            row_project,
        )?)
    }

    /// Projects by most recently opened, across all environments.
    pub fn recent_projects(&self, limit: usize) -> Result<Vec<Project>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {PROJECT_COLS} FROM projects ORDER BY last_opened_at DESC, name LIMIT ?1"
        ))?;
        let rows = st.query_map([sql_limit(limit)], row_project)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One project by id.
    pub fn get_project(&self, id: &str) -> Result<Option<Project>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1"),
                [id],
                row_project,
            )
            .optional()?)
    }

    /// The project on `env_id` that holds repository `key`, most recently opened if several.
    pub fn project_by_repo_key(&self, env_id: &str, key: &str) -> Result<Option<Project>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {PROJECT_COLS} FROM projects WHERE environment_id = ?1 AND repo_key = ?2
                     ORDER BY last_opened_at DESC LIMIT 1"
                ),
                params![env_id, key],
                row_project,
            )
            .optional()?)
    }
}

// ---------------------------------------------------------------------------
// agents: a profile (what the agent is) and its instances (RFD 0004)
// ---------------------------------------------------------------------------

/// An instance joined with its profile, in [`row_agent`] order.
const AGENT_SELECT: &str = "SELECT a.pubkey, p.name, p.harness, p.model, p.effort, p.skills_json, p.prompt, p.color,
        p.runtime_mode, a.auth_tag_json, a.created_at, a.last_seen_at, COALESCE(a.archived_at, p.archived_at) AS archived_at,
        a.profile_id, p.owner, a.device
     FROM agents a JOIN agent_profiles p ON p.profile_id = a.profile_id";

/// Instances first as people mean them: this machine's, live, recently seen.
const AGENT_ORDER: &str =
    "ORDER BY a.device IS NOT NULL, COALESCE(a.archived_at, p.archived_at) IS NOT NULL, a.last_seen_at IS NULL, a.last_seen_at DESC, a.created_at";

/// The most live profiles one name lookup reads.  Each owner has at most
/// one per name once merged; more than this many owners sharing a name on
/// one machine is not a case we serve.
const PROFILES_PER_NAME_MAX: i64 = 1_000;
/// The most profiles [`Store::name_clashes`] reads.  Linking two homes of
/// twenty agents each clashes on at most forty.
const CLASHING_PROFILES_MAX: i64 = 10_000;

const PROFILE_COLS: &str = "profile_id, owner, name, harness, model, effort, skills_json, prompt, color, runtime_mode, created_at, updated_at, updated_id, archived_at, merged_into";

fn json_error(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn row_agent(r: &Row<'_>) -> rusqlite::Result<Agent> {
    let skills_json: String = r.get("skills_json")?;
    let auth_tag_json: Option<String> = r.get("auth_tag_json")?;
    Ok(Agent {
        pubkey: r.get("pubkey")?,
        name: r.get("name")?,
        harness: Harness::parse(&r.get::<_, String>("harness")?),
        model: r.get("model")?,
        // Unknown text (an older or newer build's value) reads as "no effort" / "no colour" / the default mode.
        effort: r.get::<_, Option<String>>("effort")?.and_then(|e| e.parse().ok()),
        skills: serde_json::from_str(&skills_json).map_err(json_error)?,
        prompt: r.get("prompt")?,
        color: r.get::<_, Option<String>>("color")?.and_then(|c| c.parse().ok()),
        runtime_mode: r.get::<_, String>("runtime_mode")?.parse().unwrap_or_default(),
        auth_tag: auth_tag_json.map(|s| serde_json::from_str(&s)).transpose().map_err(json_error)?,
        created_at: r.get("created_at")?,
        last_seen_at: r.get("last_seen_at")?,
        archived_at: r.get("archived_at")?,
        profile_id: r.get("profile_id")?,
        owner: r.get("owner")?,
        device: r.get("device")?,
    })
}

fn row_profile(r: &Row<'_>) -> rusqlite::Result<AgentProfile> {
    let skills_json: String = r.get("skills_json")?;
    Ok(AgentProfile {
        profile_id: r.get("profile_id")?,
        owner: r.get("owner")?,
        name: r.get("name")?,
        harness: Harness::parse(&r.get::<_, String>("harness")?),
        model: r.get("model")?,
        effort: r.get::<_, Option<String>>("effort")?.and_then(|e| e.parse().ok()),
        skills: serde_json::from_str(&skills_json).map_err(json_error)?,
        prompt: r.get("prompt")?,
        color: r.get::<_, Option<String>>("color")?.and_then(|c| c.parse().ok()),
        runtime_mode: r.get::<_, String>("runtime_mode")?.parse().unwrap_or_default(),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        updated_id: r.get("updated_id")?,
        archived_at: r.get("archived_at")?,
        merged_into: r.get("merged_into")?,
    })
}

/// The owner an attestation names, if it parses.
fn attester(auth_tag: Option<&Vec<String>>) -> Option<String> {
    auth_tag.and_then(|t| axel_identity::AuthTag::from_tag(t).ok()).map(|t| t.owner.to_base58())
}

impl Store {
    /// Insert an agent instance, and its profile unless `a.profile_id` names
    /// one already here.  Fails with [`Error::Conflict`] when the pubkey is
    /// taken, or when a new profile's name is taken by another live profile
    /// of the same owner.
    pub fn insert_agent(&self, a: &NewAgent) -> Result<Agent, Error> {
        assert!(!a.pubkey.is_empty(), "an agent instance has a key");
        let created_at = now();
        let tx = self.conn.unchecked_transaction()?;
        let profile_id = match &a.profile_id {
            Some(id) if profile_exists(&tx, id)? => {
                self.check_same_owner(&tx, id, a.auth_tag.as_ref())?;
                id.clone()
            }
            given => {
                let id = given.clone().unwrap_or_else(|| format!("p-{}", new_id()));
                let owner = attester(a.auth_tag.as_ref()).map(|o| self.principal_in(&tx, &o)).transpose()?;
                self.check_name_free(&a.name, owner.as_deref())?;
                insert_profile(&tx, &id, owner.as_deref(), a, created_at)?;
                id
            }
        };
        let auth_tag_json = a.auth_tag.as_ref().map(serde_json::to_string).transpose()?;
        tx.execute(
            "INSERT INTO agents (pubkey, profile_id, device, auth_tag_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![a.pubkey, profile_id, a.device, auth_tag_json, created_at],
        )
        .map_err(|e| Error::conflict_or(e, || format!("agent pubkey {} already exists", a.pubkey)))?;
        tx.commit()?;
        let agent = self.agent_by_pubkey(&a.pubkey)?.ok_or_else(|| Error::NotFound(format!("agent {}", a.pubkey)))?;
        debug_assert_eq!(agent.profile_id, profile_id);
        Ok(agent)
    }

    /// Refuse a new profile named like a live one of the same owner.  Owners
    /// compare as principals: a linked home's profiles carry its own key.
    fn check_name_free(&self, name: &str, owner: Option<&str>) -> Result<(), Error> {
        for profile in self.live_profiles_named(name)? {
            let profile_owner = profile.owner.as_deref().map(|o| self.principal(o)).transpose()?;
            if profile_owner.as_deref() == owner {
                return Err(Error::Conflict(format!("agent name {name:?} already exists")));
            }
        }
        Ok(())
    }

    /// Refuse an instance whose attester doesn't speak for the owner of the
    /// profile it would join: an agent of one owner never becomes an instance
    /// of another's.
    fn check_same_owner(&self, conn: &rusqlite::Connection, profile_id: &str, auth_tag: Option<&Vec<String>>) -> Result<(), Error> {
        let owner: Option<String> = conn.query_row("SELECT owner FROM agent_profiles WHERE profile_id = ?1", [profile_id], |r| r.get(0))?;
        let attester = attester(auth_tag);
        let same = match (owner, attester) {
            (Some(owner), Some(attester)) => self.principal_in(conn, &owner)? == self.principal_in(conn, &attester)?,
            _ => false,
        };
        if !same {
            return Err(Error::Conflict(format!("profile {profile_id} belongs to another owner")));
        }
        Ok(())
    }

    fn principal_in(&self, conn: &rusqlite::Connection, key: &str) -> Result<String, Error> {
        let principals = crate::owner::Principals::load(conn)?;
        let now = u64::try_from(now()).unwrap_or(0);
        Ok(principals.resolve(key, now).to_string())
    }

    /// Non-archived agents, one row per profile: the instance on this
    /// machine when there is one, else the most recently seen.  Most
    /// recently seen first (never-seen last), then by name.
    pub fn list_agents(&self) -> Result<Vec<Agent>, Error> {
        let all = self.list_instances()?;
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Agent> = all.into_iter().filter(|a| seen.insert(a.profile_id.clone())).collect();
        out.sort_by(|x, y| {
            (x.last_seen_at.is_none(), std::cmp::Reverse(x.last_seen_at), x.name.to_lowercase())
                .cmp(&(y.last_seen_at.is_none(), std::cmp::Reverse(y.last_seen_at), y.name.to_lowercase()))
        });
        Ok(out)
    }

    /// Every non-archived instance, on this machine and on others.
    pub fn list_instances(&self) -> Result<Vec<Agent>, Error> {
        let mut st = self.conn.prepare(&format!(
            "{AGENT_SELECT} WHERE a.archived_at IS NULL AND p.archived_at IS NULL {AGENT_ORDER}"
        ))?;
        let rows = st.query_map([], row_agent)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The instances of one profile, this machine's first.
    pub fn profile_instances(&self, profile_id: &str) -> Result<Vec<Agent>, Error> {
        let mut st = self.conn.prepare(&format!("{AGENT_SELECT} WHERE a.profile_id = ?1 {AGENT_ORDER}"))?;
        let rows = st.query_map([profile_id], row_agent)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Look an agent up by name (including archived ones): the instance on
    /// this machine when there is one, else the most recently seen.
    pub fn agent_by_name(&self, name: &str) -> Result<Option<Agent>, Error> {
        Ok(self
            .conn
            .query_row(&format!("{AGENT_SELECT} WHERE p.name = ?1 {AGENT_ORDER} LIMIT 1"), [name], row_agent)
            .optional()?)
    }

    /// The instance of the profile named `name` that runs on `device`.
    pub fn agent_on_device(&self, name: &str, device: &str) -> Result<Option<Agent>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("{AGENT_SELECT} WHERE p.name = ?1 AND a.device = ?2 {AGENT_ORDER} LIMIT 1"),
                [name, device],
                row_agent,
            )
            .optional()?)
    }

    /// Look an agent up by pubkey (including archived ones).
    pub fn agent_by_pubkey(&self, pk: &str) -> Result<Option<Agent>, Error> {
        Ok(self
            .conn
            .query_row(&format!("{AGENT_SELECT} WHERE a.pubkey = ?1"), [pk], row_agent)
            .optional()?)
    }

    /// One profile by id (including an archived one).
    pub fn profile(&self, profile_id: &str) -> Result<Option<AgentProfile>, Error> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {PROFILE_COLS} FROM agent_profiles WHERE profile_id = ?1"), [profile_id], row_profile)
            .optional()?)
    }

    /// The live profile named `name`, the oldest when several owners each
    /// have one.  Two of the same owner (two homes linked, each with its
    /// own) are a choice for the owner, not for us: [`Error::Conflict`],
    /// until `axel device merge` settles it.
    pub fn profile_by_name(&self, name: &str) -> Result<Option<AgentProfile>, Error> {
        let named = self.live_profiles_named(name)?;
        if let Some(clash) = self.clashes_among(&named)?.into_iter().next() {
            let ids: Vec<&str> = clash.iter().map(|p| p.profile_id.as_str()).collect();
            return Err(Error::Conflict(format!(
                "{} agents are named {name:?} ({}); run `axel device merge {name}`",
                ids.len(),
                ids.join(", ")
            )));
        }
        Ok(named.into_iter().next())
    }

    /// Live profiles named `name`, oldest first.
    fn live_profiles_named(&self, name: &str) -> Result<Vec<AgentProfile>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {PROFILE_COLS} FROM agent_profiles WHERE name = ?1 AND archived_at IS NULL
             ORDER BY created_at, profile_id LIMIT ?2"
        ))?;
        let rows = st.query_map(params![name, PROFILES_PER_NAME_MAX], row_profile)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Groups of two or more of `profiles` whose owners are one principal,
    /// each oldest first.
    fn clashes_among(&self, profiles: &[AgentProfile]) -> Result<Vec<Vec<AgentProfile>>, Error> {
        let mut by_owner: BTreeMap<(String, String), Vec<AgentProfile>> = BTreeMap::new();
        for profile in profiles {
            // A profile with no known owner is no one's: it clashes with nothing.
            let Some(owner) = &profile.owner else { continue };
            by_owner.entry((profile.name.clone(), self.principal(owner)?)).or_default().push(profile.clone());
        }
        let mut clashes: Vec<Vec<AgentProfile>> = by_owner.into_values().filter(|group| group.len() > 1).collect();
        for group in &mut clashes {
            group.sort_by(|a, b| (a.created_at, &a.profile_id).cmp(&(b.created_at, &b.profile_id)));
        }
        Ok(clashes)
    }

    /// Every name that two or more live profiles of `owner` (a principal)
    /// share, each group oldest first: what linking two homes leaves for
    /// `axel device merge`.
    pub fn name_clashes(&self, owner: &str) -> Result<Vec<Vec<AgentProfile>>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {PROFILE_COLS} FROM agent_profiles WHERE archived_at IS NULL AND name IN
                 (SELECT name FROM agent_profiles WHERE archived_at IS NULL GROUP BY name HAVING COUNT(*) > 1)
             ORDER BY name, created_at, profile_id LIMIT ?1"
        ))?;
        let rows = st.query_map([CLASHING_PROFILES_MAX], row_profile)?;
        let shared: Vec<AgentProfile> = rows.collect::<Result<_, _>>()?;
        let clashes = self.clashes_among(&shared)?;
        let mut mine = Vec::with_capacity(clashes.len());
        for group in clashes {
            let first_owner = group[0].owner.as_deref().expect("a clash is between owned profiles");
            if self.principal(first_owner)? == owner {
                mine.push(group);
            }
        }
        Ok(mine)
    }

    /// Merge the live profile `from` into `into`, both `owner`'s (a
    /// principal): `from` is archived and remembers `into`, and every
    /// instance of `from` becomes one of `into`'s.  Unsigned until the next
    /// publish, which also announces this machine's moved instances again.
    pub fn merge_profile(&self, from: &str, into: &str, owner: &str) -> Result<(), Error> {
        if from == into {
            return Err(Error::Invalid(format!("profile {from} can't merge into itself")));
        }
        let tx = self.conn.unchecked_transaction()?;
        for id in [from, into] {
            let profile = self.profile(id)?.ok_or_else(|| Error::NotFound(format!("profile {id}")))?;
            if profile.archived_at.is_some() {
                return Err(Error::Invalid(format!("profile {id} ({}) was removed", profile.name)));
            }
            let profile_owner = profile.owner.as_deref().map(|o| self.principal(o)).transpose()?;
            if profile_owner.as_deref() != Some(owner) {
                return Err(Error::Invalid(format!("profile {id} ({}) is another owner's", profile.name)));
            }
        }
        let at = now();
        tx.execute(
            "UPDATE agent_profiles SET merged_into = ?2, archived_at = ?3, updated_at = MAX(updated_at + 1, ?3), updated_id = NULL
             WHERE profile_id = ?1",
            params![from, into, at],
        )?;
        crate::owner::move_instances(&tx, from, into)?;
        tx.commit()?;
        debug_assert!(self.profile_instances(from)?.is_empty(), "every instance moved");
        Ok(())
    }

    /// Rename the profile `profile_id`, which must stay the only live one of
    /// its owner with that name.  Unsigned until the next publish.
    pub fn rename_profile(&self, profile_id: &str, name: &str) -> Result<(), Error> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("an agent needs a name".into()));
        }
        let profile = self.profile(profile_id)?.ok_or_else(|| Error::NotFound(format!("profile {profile_id}")))?;
        let mut named = self.live_profiles_named(name)?;
        named.retain(|p| p.profile_id != profile_id);
        named.push(AgentProfile { name: name.to_string(), ..profile });
        if !self.clashes_among(&named)?.is_empty() {
            return Err(Error::Conflict(format!("agent name {name:?} already exists")));
        }
        self.conn.execute(
            "UPDATE agent_profiles SET name = ?2, updated_at = MAX(updated_at + 1, ?3), updated_id = NULL WHERE profile_id = ?1",
            params![profile_id, name, now()],
        )?;
        Ok(())
    }

    /// Every profile, archived ones included, by name.
    pub fn profiles(&self) -> Result<Vec<AgentProfile>, Error> {
        let mut st = self.conn.prepare(&format!("SELECT {PROFILE_COLS} FROM agent_profiles ORDER BY name COLLATE NOCASE, created_at"))?;
        let rows = st.query_map([], row_profile)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Profiles edited here and not yet signed, owned by `owner` (a
    /// principal): what the next publish signs.
    pub fn unsigned_profiles(&self, owner: &str) -> Result<Vec<AgentProfile>, Error> {
        let mut st = self.conn.prepare(&format!("SELECT {PROFILE_COLS} FROM agent_profiles WHERE updated_id IS NULL ORDER BY updated_at"))?;
        let rows = st.query_map([], row_profile)?;
        let all: Vec<AgentProfile> = rows.collect::<Result<_, _>>()?;
        let mut out = Vec::with_capacity(all.len());
        for p in all {
            if let Some(o) = &p.owner
                && self.principal(o)? == owner
            {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Set `last_seen_at = now`.
    pub fn touch_agent(&self, pk: &str) -> Result<(), Error> {
        let n = self.conn.execute("UPDATE agents SET last_seen_at = ?2 WHERE pubkey = ?1", params![pk, now()])?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Change one column of the profile `pk` instantiates.  The edit is
    /// unsigned until the next publish, and wins over anything older.
    fn edit_profile(&self, pk: &str, column: &str, value: rusqlite::types::Value) -> Result<(), Error> {
        debug_assert!(
            ["name", "harness", "model", "effort", "skills_json", "prompt", "color", "runtime_mode"].contains(&column),
            "{column} is a profile column"
        );
        let n = self.conn.execute(
            &format!(
                "UPDATE agent_profiles SET {column} = ?2, updated_at = MAX(updated_at + 1, ?3), updated_id = NULL
                 WHERE profile_id = (SELECT profile_id FROM agents WHERE pubkey = ?1)"
            ),
            params![pk, value, now()],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Change an agent's runtime mode (applies to sessions started afterwards).
    pub fn set_agent_runtime_mode(&self, pk: &str, mode: crate::RuntimeMode) -> Result<(), Error> {
        self.edit_profile(pk, "runtime_mode", mode.as_str().to_string().into())
    }

    /// Change an agent's harness and model (`None`: the harness's default);
    /// applies to sessions started afterwards.
    pub fn set_agent_harness(&self, pk: &str, harness: &crate::Harness, model: Option<&str>) -> Result<(), Error> {
        self.edit_profile(pk, "harness", harness.as_str().to_string().into())?;
        self.edit_profile(pk, "model", model.map(str::to_string).into())
    }

    /// Change how hard an agent's model thinks (`None`: the harness's
    /// default); applies to sessions started afterwards.
    pub fn set_agent_effort(&self, pk: &str, effort: Option<crate::Effort>) -> Result<(), Error> {
        self.edit_profile(pk, "effort", effort.map(|e| e.as_str().to_string()).into())
    }

    /// Give an agent a new name.  Its key, attestation, policy and history
    /// stay: everything is keyed by pubkey, the name is what people type.
    /// Sessions already running keep the old name in their environment.
    pub fn rename_agent(&self, pk: &str, name: &str) -> Result<(), Error> {
        assert!(!name.trim().is_empty(), "an agent has a name");
        self.edit_profile(pk, "name", name.to_string().into())
    }

    /// Change the parts of a profile that `edit` may: each `Some` replaces
    /// the field; an empty prompt clears it.  Sessions started afterwards see it.
    pub fn set_agent_profile(&self, pk: &str, skills: Option<&[String]>, prompt: Option<&str>, color: Option<crate::AgentColor>) -> Result<(), Error> {
        if self.agent_by_pubkey(pk)?.is_none() {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        if let Some(skills) = skills {
            self.edit_profile(pk, "skills_json", serde_json::to_string(skills)?.into())?;
        }
        if let Some(prompt) = prompt {
            let prompt = (!prompt.trim().is_empty()).then(|| prompt.to_string());
            self.edit_profile(pk, "prompt", prompt.into())?;
        }
        if let Some(color) = color {
            self.edit_profile(pk, "color", color.as_str().to_string().into())?;
        }
        Ok(())
    }

    /// Soft-delete an agent everywhere: its profile is archived (and signed
    /// as such on the next publish), and so is this instance.  The rows stay.
    pub fn archive_agent(&self, pk: &str) -> Result<(), Error> {
        let at = now();
        let n = self.conn.execute(
            "UPDATE agents SET archived_at = COALESCE(archived_at, ?2) WHERE pubkey = ?1",
            params![pk, at],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        self.conn.execute(
            "UPDATE agent_profiles SET archived_at = COALESCE(archived_at, ?2), updated_at = MAX(updated_at + 1, ?2), updated_id = NULL
             WHERE profile_id = (SELECT profile_id FROM agents WHERE pubkey = ?1)",
            params![pk, at],
        )?;
        Ok(())
    }

    /// A display name for a pubkey: the agent's name if it is an instance
    /// we know, or a device's label.  (Human participants carry no name in
    /// the log yet.)
    pub fn known_identity_name(&self, pubkey: &str) -> Result<Option<String>, Error> {
        let agent: Option<String> = self
            .conn
            .query_row(
                "SELECT p.name FROM agents a JOIN agent_profiles p ON p.profile_id = a.profile_id WHERE a.pubkey = ?1",
                [pubkey],
                |r| r.get(0),
            )
            .optional()?;
        match agent {
            Some(name) => Ok(Some(name)),
            None => Ok(self.device(pubkey)?.map(|d| d.label)),
        }
    }
}

fn profile_exists(conn: &rusqlite::Connection, profile_id: &str) -> Result<bool, Error> {
    Ok(conn
        .query_row("SELECT 1 FROM agent_profiles WHERE profile_id = ?1", [profile_id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// A new profile from `a`'s fields.  Refused when a live profile of the same
/// owner already has the name: names are unique per owner.
/// Insert a profile whose name [`Store::check_name_free`] checked.
fn insert_profile(conn: &rusqlite::Connection, id: &str, owner: Option<&str>, a: &NewAgent, at: i64) -> Result<(), Error> {
    assert!(!a.name.trim().is_empty(), "an agent has a name");
    conn.execute(
        "INSERT INTO agent_profiles (profile_id, owner, name, harness, model, effort, skills_json, prompt, color, runtime_mode, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        params![
            id,
            owner,
            a.name,
            a.harness.as_str(),
            a.model,
            a.effort.map(|e| e.as_str()),
            serde_json::to_string(&a.skills)?,
            a.prompt,
            a.color.map(|c| c.as_str()),
            a.runtime_mode.as_str(),
            at
        ],
    )?;
    Ok(())
}
