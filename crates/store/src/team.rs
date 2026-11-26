//! Teams (RFD 0005): a named set of the owner's agent profiles, with skill
//! sources and a reach rule they share.  Like profiles, a team is the
//! owner's and the same on every device: it is projected from kind-123
//! envelopes (see `owner.rs`), and a local edit stays unsigned
//! (`updated_id` NULL) until the owner publishes it.
//!
//! Axel stores, shows and installs what a team says; it never routes by role.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::owner::Principals;
use crate::{Error, NewTeam, Store, Team, TeamEdit, TeamMember, TeamReach, new_id, now, sql_limit};

/// The most members a team has.  Issue #40's team, the largest anyone has
/// asked for, is nine; 32 is over three times that, and keeps the roster
/// written into every member's system prompt to a few dozen short lines.
pub const TEAM_MEMBERS_MAX: usize = 32;

/// The most skill sources a team lists.  A source is a directory or a
/// repository of many skills, so a handful covers a team; 16 bounds a
/// launch's fetches.
pub const TEAM_SKILLS_MAX: usize = 16;

/// The longest team name, in bytes.  Names are typed and shown in one
/// column, like agent names.
pub const TEAM_NAME_BYTES_MAX: usize = 64;

/// The longest role, in bytes: one line of the roster.
pub const TEAM_ROLE_BYTES_MAX: usize = 200;

/// The longest skill source, in bytes: a path or a GitHub URL with a ref.
pub const TEAM_SKILL_SOURCE_BYTES_MAX: usize = 512;

/// The most teams one read returns.  An owner has a few teams; 256 bounds
/// every listing and every launch's walk over a profile's teams.
pub const TEAMS_MAX: usize = 256;

const TEAM_COLS: &str = "team_id, owner, name, skills_json, reach, lead, created_at, updated_at, updated_id, archived_at";

/// A team name as people type it: letters, digits, `-` and `_`, like agent
/// names, so `@name` stays open for teams later.
pub fn is_valid_team_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= TEAM_NAME_BYTES_MAX
        && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Why a team's shape is refused: limits, duplicates, a lead outside the
/// team.  The same check for a local edit and for a record from elsewhere.
pub(crate) fn check_shape(name: &str, members: &[TeamMember], skills: &[String], lead: Option<&str>) -> Result<(), String> {
    if !is_valid_team_name(name) {
        return Err(format!("team name {name:?} is not letters, digits, - and _ (at most {TEAM_NAME_BYTES_MAX} bytes)"));
    }
    if members.len() > TEAM_MEMBERS_MAX {
        return Err(format!("a team has at most {TEAM_MEMBERS_MAX} members, not {}", members.len()));
    }
    if skills.len() > TEAM_SKILLS_MAX {
        return Err(format!("a team lists at most {TEAM_SKILLS_MAX} skill sources, not {}", skills.len()));
    }
    for (index, member) in members.iter().enumerate() {
        if member.profile_id.is_empty() {
            return Err("a member needs a profile id".into());
        }
        if member.role.len() > TEAM_ROLE_BYTES_MAX || member.role.contains('\n') {
            return Err(format!("a role is one line of at most {TEAM_ROLE_BYTES_MAX} bytes"));
        }
        if members[..index].iter().any(|m| m.profile_id == member.profile_id) {
            return Err(format!("profile {} is a member twice", member.profile_id));
        }
    }
    if let Some(source) = skills.iter().find(|s| s.trim().is_empty() || s.len() > TEAM_SKILL_SOURCE_BYTES_MAX) {
        return Err(format!("skill source {source:?} is empty or longer than {TEAM_SKILL_SOURCE_BYTES_MAX} bytes"));
    }
    if let Some(lead) = lead
        && !members.iter().any(|m| m.profile_id == lead)
    {
        return Err(format!("lead {lead} is not a member"));
    }
    Ok(())
}

/// Why `members` can't be in a team of `owner` (a principal) at `at`: a
/// profile not received yet, or one another owner's.  A record from
/// elsewhere that fails here is revived once the profile arrives.
pub(crate) fn check_members(
    conn: &Connection,
    principals: &Principals,
    owner: &str,
    members: &[TeamMember],
    at: u64,
) -> Result<Result<(), String>, Error> {
    assert!(members.len() <= TEAM_MEMBERS_MAX, "the shape is checked before the members");
    for member in members {
        let profile_owner: Option<Option<String>> = conn
            .query_row("SELECT owner FROM agent_profiles WHERE profile_id = ?1", [&member.profile_id], |r| r.get(0))
            .optional()?;
        let Some(profile_owner) = profile_owner else {
            return Ok(Err(format!("profile {} not received yet", member.profile_id)));
        };
        let same_owner = profile_owner.is_some_and(|o| principals.resolve(&o, at) == owner);
        if !same_owner {
            return Ok(Err(format!("profile {} is not {owner}'s", member.profile_id)));
        }
    }
    Ok(Ok(()))
}

fn json_error(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

/// A team row without its members; [`read_members`] fills them in.
fn row_team(r: &Row<'_>) -> rusqlite::Result<Team> {
    let skills_json: String = r.get("skills_json")?;
    Ok(Team {
        team_id: r.get("team_id")?,
        owner: r.get("owner")?,
        name: r.get("name")?,
        members: Vec::new(),
        skills: serde_json::from_str(&skills_json).map_err(json_error)?,
        // Text from a newer build reads as no rule: the member's own policy
        // still applies, so an unknown rule never widens anyone's reach.
        reach: r.get::<_, String>("reach")?.parse().unwrap_or_default(),
        lead: r.get("lead")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        updated_id: r.get("updated_id")?,
        archived_at: r.get("archived_at")?,
    })
}

fn read_members(conn: &Connection, team_id: &str) -> Result<Vec<TeamMember>, Error> {
    let mut statement = conn.prepare_cached(
        "SELECT profile_id, role FROM team_members WHERE team_id = ?1 ORDER BY position LIMIT ?2",
    )?;
    let rows = statement.query_map(params![team_id, sql_limit(TEAM_MEMBERS_MAX)], |r| {
        Ok(TeamMember { profile_id: r.get(0)?, role: r.get(1)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Teams matching `filter` (a `WHERE` clause over `teams`, with `?1` bound
/// to `arg`), members filled in, at most [`TEAMS_MAX`].
fn read_teams(conn: &Connection, filter: &str, arg: &str) -> Result<Vec<Team>, Error> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {TEAM_COLS} FROM teams WHERE {filter} ORDER BY name COLLATE NOCASE, created_at, team_id LIMIT ?2"
    ))?;
    let rows = statement.query_map(params![arg, sql_limit(TEAMS_MAX)], row_team)?;
    let mut teams: Vec<Team> = rows.collect::<Result<_, _>>()?;
    for team in &mut teams {
        team.members = read_members(conn, &team.team_id)?;
    }
    debug_assert!(teams.len() <= TEAMS_MAX);
    Ok(teams)
}

pub(crate) fn read_team(conn: &Connection, team_id: &str) -> Result<Option<Team>, Error> {
    Ok(read_teams(conn, "team_id = ?1", team_id)?.into_iter().next())
}

/// Write `team` whole: the row (keeping the first `created_at`) and its
/// members, replacing the old ones.
pub(crate) fn write_team(conn: &Connection, team: &Team) -> Result<(), Error> {
    assert!(team.members.len() <= TEAM_MEMBERS_MAX, "the shape is checked before writing");
    assert!(team.lead.as_deref().is_none_or(|l| team.has_member(l)), "the lead is a member");
    conn.execute(
        "INSERT INTO teams (team_id, owner, name, skills_json, reach, lead, created_at, updated_at, updated_id, archived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(team_id) DO UPDATE SET owner = excluded.owner, name = excluded.name, skills_json = excluded.skills_json,
             reach = excluded.reach, lead = excluded.lead, updated_at = excluded.updated_at,
             updated_id = excluded.updated_id, archived_at = excluded.archived_at",
        params![
            team.team_id,
            team.owner,
            team.name,
            serde_json::to_string(&team.skills)?,
            team.reach.as_str(),
            team.lead,
            team.created_at,
            team.updated_at,
            team.updated_id,
            team.archived_at,
        ],
    )?;
    conn.execute("DELETE FROM team_members WHERE team_id = ?1", [&team.team_id])?;
    for (position, member) in team.members.iter().enumerate() {
        conn.execute(
            "INSERT INTO team_members (team_id, profile_id, role, position) VALUES (?1, ?2, ?3, ?4)",
            params![team.team_id, member.profile_id, member.role, sql_limit(position)],
        )?;
    }
    Ok(())
}

/// `members` with `lead` added (role empty) when it isn't one of them.
fn with_lead(mut members: Vec<TeamMember>, lead: Option<&str>) -> Vec<TeamMember> {
    if let Some(lead) = lead
        && !members.iter().any(|m| m.profile_id == lead)
    {
        members.push(TeamMember { profile_id: lead.to_string(), role: String::new() });
    }
    members
}

/// Apply `edit` to `team`: removals, then additions (a member already
/// there gets the new role), then lead, skills and reach.
fn edited(team: &Team, edit: &TeamEdit) -> Result<Team, Error> {
    let mut out = team.clone();
    let removes_lead = team.lead.as_ref().is_some_and(|l| edit.remove.contains(l));
    if removes_lead && edit.lead.as_ref().is_none_or(|l| edit.remove.contains(l)) {
        return Err(Error::Invalid(format!("removing the lead of {} needs another lead", team.name)));
    }
    out.members.retain(|m| !edit.remove.contains(&m.profile_id));
    for add in &edit.add {
        match out.members.iter_mut().find(|m| m.profile_id == add.profile_id) {
            Some(existing) => existing.role.clone_from(&add.role),
            None => out.members.push(add.clone()),
        }
    }
    if let Some(lead) = &edit.lead {
        out.lead = Some(lead.clone());
    }
    out.members = with_lead(out.members, out.lead.as_deref());
    if let Some(skills) = &edit.skills {
        out.skills.clone_from(skills);
    }
    if let Some(reach) = edit.reach {
        out.reach = reach;
    }
    Ok(out)
}

impl Store {
    /// Refuse a team that can't be saved here: its shape, members that
    /// aren't this owner's profiles, or a `lead` rule with no lead.
    fn check_local_team(&self, team: &Team) -> Result<(), Error> {
        check_shape(&team.name, &team.members, &team.skills, team.lead.as_deref()).map_err(Error::Invalid)?;
        if team.reach == TeamReach::Lead && team.lead.is_none() {
            return Err(Error::Invalid(format!("team {} reaches by its lead but has none", team.name)));
        }
        let principals = Principals::load(&self.conn)?;
        // A clock before 1970 resolves links as of the epoch: no device link
        // is that old, so the owner key itself must match.
        let at = u64::try_from(now()).unwrap_or(0);
        let owner = principals.resolve(&team.owner, at);
        check_members(&self.conn, &principals, owner, &team.members, at)?.map_err(Error::Invalid)
    }

    /// Create a team, unsigned until published.  Fails with
    /// [`Error::Conflict`] when the owner already has a live team of that
    /// name, and [`Error::Invalid`] past a limit or for a member that is no
    /// profile of the owner's.
    pub fn create_team(&self, new: &NewTeam) -> Result<Team, Error> {
        assert!(!new.owner.is_empty(), "a team has an owner");
        let at = now();
        let team = Team {
            team_id: format!("t-{}", new_id()),
            owner: new.owner.clone(),
            name: new.name.trim().to_string(),
            members: with_lead(new.members.clone(), new.lead.as_deref()),
            skills: new.skills.clone(),
            reach: new.reach,
            lead: new.lead.clone(),
            created_at: at,
            updated_at: at,
            updated_id: None,
            archived_at: None,
        };
        self.check_local_team(&team)?;
        let taken = read_teams(&self.conn, "name = ?1 AND archived_at IS NULL", &team.name)?
            .iter()
            .any(|t| self.principal(&t.owner).is_ok_and(|o| o == team.owner));
        if taken {
            return Err(Error::Conflict(format!("a team named {:?} already exists", team.name)));
        }
        write_team(&self.conn, &team)?;
        debug_assert_eq!(self.team(&team.team_id)?.as_ref(), Some(&team));
        Ok(team)
    }

    /// Change a live team.  The edit is unsigned until the next publish and
    /// wins over anything older.
    pub fn edit_team(&self, team_id: &str, edit: &TeamEdit) -> Result<Team, Error> {
        let team = self.team(team_id)?.ok_or_else(|| Error::NotFound(format!("team {team_id}")))?;
        if team.archived_at.is_some() {
            return Err(Error::Invalid(format!("team {} is archived", team.name)));
        }
        let mut out = edited(&team, edit)?;
        out.updated_at = team.updated_at.saturating_add(1).max(now());
        out.updated_id = None;
        self.check_local_team(&out)?;
        write_team(&self.conn, &out)?;
        assert!(out.updated_at > team.updated_at, "an edit is newer than what it changes");
        Ok(out)
    }

    /// Archive a team everywhere (signed as such on the next publish): its
    /// policy and skills stop applying.  The row stays.
    pub fn archive_team(&self, team_id: &str) -> Result<Team, Error> {
        let at = now();
        let changed = self.conn.execute(
            "UPDATE teams SET archived_at = COALESCE(archived_at, ?2), updated_at = MAX(updated_at + 1, ?2), updated_id = NULL
             WHERE team_id = ?1",
            params![team_id, at],
        )?;
        if changed == 0 {
            return Err(Error::NotFound(format!("team {team_id}")));
        }
        let team = self.team(team_id)?.ok_or_else(|| Error::NotFound(format!("team {team_id}")))?;
        assert!(team.archived_at.is_some(), "the team was just archived");
        Ok(team)
    }

    /// One team by id, archived or not.
    pub fn team(&self, team_id: &str) -> Result<Option<Team>, Error> {
        read_team(&self.conn, team_id)
    }

    /// The live team named `name`, the oldest when two devices made one
    /// each while apart.
    pub fn team_by_name(&self, name: &str) -> Result<Option<Team>, Error> {
        let mut teams = read_teams(&self.conn, "name = ?1 AND archived_at IS NULL", name)?;
        teams.sort_by_key(|t| (t.created_at, t.team_id.clone()));
        Ok(teams.into_iter().next())
    }

    /// Every live team, by name.
    pub fn teams(&self) -> Result<Vec<Team>, Error> {
        read_teams(&self.conn, "archived_at IS NULL AND ?1 = ?1", "")
    }

    /// The live teams `profile_id` is a member of, by name.
    pub fn teams_of_profile(&self, profile_id: &str) -> Result<Vec<Team>, Error> {
        read_teams(
            &self.conn,
            "archived_at IS NULL AND team_id IN (SELECT team_id FROM team_members WHERE profile_id = ?1)",
            profile_id,
        )
    }

    /// Teams edited here and not yet signed, owned by `owner` (a
    /// principal), archived ones included: what the next publish signs.
    pub fn unsigned_teams(&self, owner: &str) -> Result<Vec<Team>, Error> {
        let all = read_teams(&self.conn, "updated_id IS NULL AND ?1 = ?1", "")?;
        let mut out = Vec::with_capacity(all.len());
        for team in all {
            if self.principal(&team.owner)? == owner {
                out.push(team);
            }
        }
        Ok(out)
    }
}
