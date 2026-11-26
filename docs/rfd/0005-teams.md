# RFD 0005: Teams: a named set of agent profiles with skills of its own

State: idea
Authors: Ludo  ·  Started: 2026-11-25

## Problem

Issue #40 runs a team of nine agents: a coordinator that delegates, two implementers,
two reviewers on another model family, four read-only helpers. Axel has no word for
that team. It lives in the coordinator's prompt and in nine `axel agent edit --reach
… --allow …` calls kept in step by hand. Skills come at three levels today: global
(`~/.config/axel/skills`), project (`AXEL.md`) and agent (the agent's skill list).
A skill every reviewer and implementer of one team should load has to be added to each
agent, and drifts when one is forgotten.

We want a team to be a thing Axel knows, without Axel learning about roles or
workflows. The orchestration stays text, in prompts and skills.

## Sketch

A team is the owner's, like an agent profile (RFD 0004): a named set of profiles plus
what they share.

```
team "core"
  members   coordinator, implementer, implementer-fast, reviewer-spec,
            reviewer-standards, scout, researcher, auditor, advisor
  roles     free text per member, shown in the dashboard and in the roster
  skills    ./skills/core            installed for every member
  policy    reach: team | lead       lead: coordinator
```

- **Members are profiles, not instances.** A team doesn't care which machine a member
  runs on. A profile may belong to several teams.
- **Team skills** are a fourth level, between project and agent: global, project, team,
  agent. They are installed whenever a member runs, from every team it belongs to.
  Sources use the `skills:` syntax `AXEL.md` already parses (`SkillSource`).
- **Roles are text.** Axel shows them and writes the roster ("you are in team core;
  reviewer-spec reviews against the spec; …") into each member's appended prompt. It
  never acts on a role.
- **Policy template.** Two new values for reach, so nine edits become one:
  - `team`: a member may reach the other members of its teams.
  - `lead`: only the team's lead may reach members; members reach the lead alone.
  An agent's own `--allow` and `--reach` still apply on top, and the stricter wins.
  #40's setup is `lead` with `coordinator` as lead.
- **Wire.** `kind 123 team`, signed by a device of the owner, content `{team_id, name,
  members: [{profile_id, role}], skills, policy, lead}`, tag `["d", team_id]`. Latest
  wins per `team_id`, as for profiles.
- **Commands.** `axel team new core --lead coordinator --member scout:"code
  reconnaissance" …`, `axel team edit`, `axel team show`, `axel team ls`.

Not in scope: `@core` as an address, and teams spanning owners.

## Open questions

- **Does a team need RFD 0004 first?** Teams name profiles, and profiles arrive in its
  phase 3. Until then a member can be an agent pubkey, migrated to its profile later.
  Decide by checking whether phase 3 lands before anyone builds this.
- **Should `@core` mean anything?** Sending to the lead is the obvious meaning. A
  broadcast is the other. Wait for a real use.
- **Thread-scoped teams?** Membership-based skills load the team's skills even when a
  member works alone. `axel thread new --team core` would scope them to a thread.
  Find out from #40's setup whether members ever work outside the team.

## Risks

- **A team becomes a workflow engine.** Roles that start to mean something to Axel
  would undo the agnosticism #40 likes. The rule: Axel stores, shows and installs;
  it never routes by role.
- **Policy gets harder to read.** Team policy plus agent policy is two places to look.
  `axel agent show` must print the effective reach and say where each part came from.

## What we learned

- 2026-11-25: The owner asked for team-level skills in #40, next to user, project and
  agent skills. Jon Leopard offered an example-team docs page, which can describe
  today's per-agent setup and move to `axel team` once it exists.
