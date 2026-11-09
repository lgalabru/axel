//! Local-only tables: environments, projects, agents.  Never envelopes.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Row, params};

use crate::{Agent, Environment, Error, Harness, NewAgent, Project, Store, new_id, now};

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

fn row_environment(r: &Row<'_>) -> rusqlite::Result<Environment> {
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

const ENV_COLS: &str = "id, label, rex_host_id, is_local, created_at, ssh_target, host_pubkey";

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
            params![new_id(), env_id, path_str, project_name(path), repo_key, has_axel_md as i64, now()],
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
        let rows = st.query_map([limit as i64], row_project)?;
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
// agents
// ---------------------------------------------------------------------------

const AGENT_COLS: &str = "pubkey, name, harness, model, skills_json, prompt, color, runtime_mode, auth_tag_json, created_at, last_seen_at, archived_at";

fn row_agent(r: &Row<'_>) -> rusqlite::Result<Agent> {
    let skills_json: String = r.get("skills_json")?;
    let auth_tag_json: Option<String> = r.get("auth_tag_json")?;
    let json_err = |e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    };
    Ok(Agent {
        pubkey: r.get("pubkey")?,
        name: r.get("name")?,
        harness: Harness::parse(&r.get::<_, String>("harness")?),
        model: r.get("model")?,
        skills: serde_json::from_str(&skills_json).map_err(json_err)?,
        prompt: r.get("prompt")?,
        // Unknown text (an older or newer build's value) reads as "no colour" / the default mode.
        color: r
            .get::<_, Option<String>>("color")?
            .and_then(|c| c.parse().ok()),
        runtime_mode: r
            .get::<_, String>("runtime_mode")?
            .parse()
            .unwrap_or_default(),
        auth_tag: auth_tag_json
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(json_err)?,
        created_at: r.get("created_at")?,
        last_seen_at: r.get("last_seen_at")?,
        archived_at: r.get("archived_at")?,
    })
}

impl Store {
    /// Insert an agent.  Fails with [`Error::Conflict`] when the pubkey or the
    /// name is already taken.
    pub fn insert_agent(&self, a: &NewAgent) -> Result<Agent, Error> {
        let skills_json = serde_json::to_string(&a.skills)?;
        let auth_tag_json = a.auth_tag.as_ref().map(serde_json::to_string).transpose()?;
        let created_at = now();
        self.conn
            .execute(
                "INSERT INTO agents (pubkey, name, harness, model, skills_json, prompt, color, runtime_mode, auth_tag_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    a.pubkey,
                    a.name,
                    a.harness.as_str(),
                    a.model,
                    skills_json,
                    a.prompt,
                    a.color.map(|c| c.as_str()),
                    a.runtime_mode.as_str(),
                    auth_tag_json,
                    created_at
                ],
            )
            .map_err(|e| {
                Error::conflict_or(e, || format!("agent name {:?} or pubkey {} already exists", a.name, a.pubkey))
            })?;
        Ok(Agent {
            pubkey: a.pubkey.clone(),
            name: a.name.clone(),
            harness: a.harness.clone(),
            model: a.model.clone(),
            skills: a.skills.clone(),
            prompt: a.prompt.clone(),
            color: a.color,
            runtime_mode: a.runtime_mode,
            auth_tag: a.auth_tag.clone(),
            created_at,
            last_seen_at: None,
            archived_at: None,
        })
    }

    /// Non-archived agents, most recently seen first (never-seen last), then by name.
    pub fn list_agents(&self) -> Result<Vec<Agent>, Error> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE archived_at IS NULL
             ORDER BY last_seen_at IS NULL, last_seen_at DESC, name COLLATE NOCASE"
        ))?;
        let rows = st.query_map([], row_agent)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Look an agent up by its unique name (including archived ones).
    pub fn agent_by_name(&self, name: &str) -> Result<Option<Agent>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {AGENT_COLS} FROM agents WHERE name = ?1"),
                [name],
                row_agent,
            )
            .optional()?)
    }

    /// Look an agent up by pubkey (including archived ones).
    pub fn agent_by_pubkey(&self, pk: &str) -> Result<Option<Agent>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {AGENT_COLS} FROM agents WHERE pubkey = ?1"),
                [pk],
                row_agent,
            )
            .optional()?)
    }

    /// Set `last_seen_at = now`.
    pub fn touch_agent(&self, pk: &str) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE agents SET last_seen_at = ?2 WHERE pubkey = ?1",
            params![pk, now()],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Change an agent's runtime mode (applies to sessions started afterwards).
    pub fn set_agent_runtime_mode(&self, pk: &str, mode: crate::RuntimeMode) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE agents SET runtime_mode = ?2 WHERE pubkey = ?1",
            params![pk, mode.as_str()],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Change an agent's harness and model (`None`: the harness's default);
    /// applies to sessions started afterwards.
    pub fn set_agent_harness(&self, pk: &str, harness: &crate::Harness, model: Option<&str>) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE agents SET harness = ?2, model = ?3 WHERE pubkey = ?1",
            params![pk, harness.as_str(), model],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Give an agent a new name.  Its key, attestation, policy and history
    /// stay: everything is keyed by pubkey, the name is what people type.
    /// Sessions already running keep the old name in their environment.
    pub fn rename_agent(&self, pk: &str, name: &str) -> Result<(), Error> {
        let n = self.conn.execute("UPDATE agents SET name = ?2 WHERE pubkey = ?1", params![pk, name])?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Change the parts of a profile that `edit` may: each `Some` replaces
    /// the field; an empty prompt clears it.  Sessions started afterwards see it.
    pub fn set_agent_profile(&self, pk: &str, skills: Option<&[String]>, prompt: Option<&str>, color: Option<crate::AgentColor>) -> Result<(), Error> {
        if let Some(skills) = skills {
            self.conn.execute("UPDATE agents SET skills_json = ?2 WHERE pubkey = ?1", params![pk, serde_json::to_string(skills)?])?;
        }
        if let Some(prompt) = prompt {
            let prompt = (!prompt.trim().is_empty()).then_some(prompt);
            self.conn.execute("UPDATE agents SET prompt = ?2 WHERE pubkey = ?1", params![pk, prompt])?;
        }
        if let Some(color) = color {
            self.conn.execute("UPDATE agents SET color = ?2 WHERE pubkey = ?1", params![pk, color.as_str()])?;
        }
        if self.agent_by_pubkey(pk)?.is_none() {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// Soft-delete: sets `archived_at`; the row (and its name) stay reserved.
    pub fn archive_agent(&self, pk: &str) -> Result<(), Error> {
        let n = self.conn.execute(
            "UPDATE agents SET archived_at = COALESCE(archived_at, ?2) WHERE pubkey = ?1",
            params![pk, now()],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("agent {pk}")));
        }
        Ok(())
    }

    /// A display name for a pubkey: the local agent's name if it is one of ours.
    /// (Human participants carry no name in the log yet.)
    pub fn known_identity_name(&self, pubkey: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row("SELECT name FROM agents WHERE pubkey = ?1", [pubkey], |r| {
                r.get(0)
            })
            .optional()?)
    }
}
