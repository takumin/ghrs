//! Assembling the `plan`. Fetches the current state, runs name resolution, and computes the diff,
//! in that order.
//!
//! `apply` uses this result as is.

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use crate::access;
use crate::actions;
use crate::auth::SelfApp;
use crate::cli::Resource;
use crate::diagnostic::{Code, Diagnostic, Diagnostics};
use crate::diff::{self, Action, Op, ResourceChange};
use crate::github::GitHub;
use crate::model::{Config, Rules};
use crate::repo::RepoRef;
use crate::resolve::{self, Directory};
use crate::schema::{MAX_RULESETS, actor_type_to_toml};
use crate::state::{self, RulesetState};
use crate::undeclared;
use crate::validate;

#[derive(Serialize)]
pub struct Plan {
    pub repository: String,
    pub changes: Vec<ResourceChange>,
    /// Resources not declared but kept because `--prune` is absent (in the form `ruleset "x"`)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<String>,
    /// Settings ghrs knows but the file leaves unwritten and meta.unmanaged does not list, kept
    /// at their current values (key paths such as `repository.homepage`)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub undeclared: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub summary: Summary,
    /// Number of write requests apply sends
    pub writes: usize,
    /// Writes apply performs, ordered by apply order.
    #[serde(skip)]
    pub steps: Vec<Step>,
}

pub enum Step {
    Repository {
        /// Fields with changes, sent to `PATCH /repos/{o}/{r}`
        fields: Map<String, Json>,
        /// topics are fully replaced via a separate endpoint
        topics: Option<Json>,
    },
    /// Bodies for each Actions endpoint with changes, sent in this order: `selected_actions`
    /// needs `allowed_actions = "selected"` to be set first
    Actions {
        permissions: Option<Json>,
        selected_actions: Option<Json>,
        workflow: Option<Json>,
    },
    GrantCollaborator {
        login: String,
        permission: String,
    },
    UpdateInvitation {
        login: String,
        id: u64,
        permission: String,
    },
    RevokeCollaborator {
        login: String,
    },
    DeleteInvitation {
        login: String,
        id: u64,
    },
    GrantTeam {
        slug: String,
        permission: String,
    },
    RevokeTeam {
        slug: String,
    },
    CreateRuleset {
        name: String,
        body: Json,
    },
    UpdateRuleset {
        name: String,
        id: u64,
        body: Json,
    },
    DeleteRuleset {
        name: String,
        id: u64,
    },
}

impl Step {
    pub fn label(&self) -> String {
        match self {
            Step::Repository { .. } => "repository".to_string(),
            Step::Actions { .. } => "actions".to_string(),
            Step::GrantCollaborator { login, permission } => {
                format!("grant {permission} to collaborator \"{login}\"")
            }
            Step::UpdateInvitation {
                login, permission, ..
            } => {
                format!("change invitation of collaborator \"{login}\" to {permission}")
            }
            Step::RevokeCollaborator { login } => format!("remove collaborator \"{login}\""),
            Step::DeleteInvitation { login, .. } => {
                format!("cancel invitation of collaborator \"{login}\"")
            }
            Step::GrantTeam { slug, permission } => {
                format!("grant {permission} to team \"{slug}\"")
            }
            Step::RevokeTeam { slug } => format!("remove team \"{slug}\""),
            Step::CreateRuleset { name, .. } => format!("create ruleset \"{name}\""),
            Step::UpdateRuleset { name, .. } => format!("update ruleset \"{name}\""),
            Step::DeleteRuleset { name, .. } => format!("delete ruleset \"{name}\""),
        }
    }

    pub fn is_ruleset(&self) -> bool {
        matches!(
            self,
            Step::CreateRuleset { .. } | Step::UpdateRuleset { .. } | Step::DeleteRuleset { .. }
        )
    }

    /// Whether a failure breaks the precondition for applying subsequent rulesets.
    /// Some settings may not allow a ruleset to be created unless visibility changes.
    ///
    /// Granting team permissions is not a precondition. A team without access to the repository
    /// can still be placed in bypass_actors (observed).
    pub fn gates_rulesets(&self) -> bool {
        matches!(self, Step::Repository { fields, .. } if fields.contains_key("visibility"))
    }

    /// Number of requests this step sends.
    pub fn writes(&self) -> usize {
        match self {
            Step::Repository { fields, topics } => {
                usize::from(!fields.is_empty()) + usize::from(topics.is_some())
            }
            Step::Actions {
                permissions,
                selected_actions,
                workflow,
            } => [permissions, selected_actions, workflow]
                .iter()
                .filter(|body| body.is_some())
                .count(),
            _ => 1,
        }
    }
}

#[derive(Serialize)]
pub struct Summary {
    pub add: usize,
    pub change: usize,
    pub destroy: usize,
}

/// Targets narrowed by `--only`. Kinds outside the scope trigger no API calls at all, and their
/// declarations are not validated against the repository.
pub struct Scope {
    pub repository: bool,
    pub actions: bool,
    pub rulesets: bool,
    pub collaborators: bool,
    pub teams: bool,
}

impl Scope {
    pub fn new(only: &[Resource]) -> Self {
        let wants = |r: fn(&Resource) -> bool| only.is_empty() || only.iter().any(r);
        Self {
            repository: wants(|r| matches!(r, Resource::Repository)),
            actions: wants(|r| matches!(r, Resource::Actions)),
            rulesets: wants(|r| matches!(r, Resource::Ruleset)),
            collaborators: wants(|r| matches!(r, Resource::Collaborator)),
            teams: wants(|r| matches!(r, Resource::Team)),
        }
    }
}

pub async fn run(
    gh: &mut GitHub,
    config: &Config,
    target: &RepoRef,
    only: &[Resource],
    prune: bool,
    self_app: Option<&SelfApp>,
) -> Result<Plan> {
    let scope = Scope::new(only);
    let mut warnings = Vec::new();

    // Owner type and visibility are also needed to validate rulesets, so fetch them regardless
    // of --only
    let repo = gh.get(&format!("/repos/{target}")).await?;
    let owned_by_org = repo["owner"]["type"] == "Organization";
    let visibility = repo["visibility"].as_str().unwrap_or_default();
    let mut errors =
        validate::check_context(config, &target.owner, owned_by_org, visibility, &scope);
    let (mut undeclared, unused) = undeclared::find(config, owned_by_org);
    undeclared.retain(|path| undeclared::in_scope(path, &scope));
    errors.extend(unused);
    if !errors.is_empty() {
        return Err(Diagnostics::new(errors).into());
    }

    let mut changes = Vec::new();
    let mut steps = Vec::new();
    if scope.repository {
        let change = diff::repository(&config.repository, &repo);
        if let Some(step) = repository_step(config, &change) {
            steps.push(step);
        }
        changes.push(change);
    }
    // Nothing to compare when nothing is declared, so spare the API calls
    if scope.actions && !config.actions.is_empty() {
        let with_selected = config.actions.selected_actions.is_some();
        let current = actions::current(gh, target, with_selected).await?;
        let change = diff::actions(&config.actions, &current);
        steps.extend(actions::step(&config.actions, &current, &change));
        changes.push(change);
    }

    let mut ignored = Vec::new();

    // Apply order is repository -> actions -> collaborator / team -> ruleset
    if scope.collaborators {
        let current = access::current_collaborators(gh, target).await?;
        let declared: Vec<(&str, &str)> = config
            .collaborators
            .iter()
            .map(|c| (c.login.as_str(), c.permission.as_str()))
            .collect();
        let d = access::diff("collaborator", &declared, &current, prune);
        changes.extend(d.changes);
        steps.extend(d.steps);
        ignored.extend(d.ignored);
        warnings.extend(d.warnings);
    }
    // Teams exist only in Organizations. Don't call the API for personal repositories
    if scope.teams && owned_by_org {
        let current = access::current_teams(gh, target).await?;
        let declared: Vec<(&str, &str)> = config
            .teams
            .iter()
            .map(|t| (t.slug.as_str(), t.permission.as_str()))
            .collect();
        let d = access::diff("team", &declared, &current, prune);
        changes.extend(d.changes);
        steps.extend(d.steps);
        ignored.extend(d.ignored);
        warnings.extend(d.warnings);
    }

    if scope.rulesets {
        let current = current_rulesets(gh, target, &mut warnings).await?;

        let mut dir = Directory::default();
        if let Some(app) = self_app {
            dir.add_self(app);
        }
        resolve::declared(gh, target, config, &mut dir).await?;

        let mut desired = Vec::new();
        let mut errors = Vec::new();
        for decl in &config.rulesets {
            let have = current.iter().find(|c| c.name == decl.name);
            match state::desired(decl, have, &dir) {
                Ok(state) => desired.push(state),
                // Report the declaration errors of every ruleset at once
                Err(err) => match err.downcast::<Diagnostics>() {
                    Ok(d) => errors.extend(d.diagnostics),
                    Err(err) => return Err(err),
                },
            }
        }
        if !errors.is_empty() {
            return Err(Diagnostics::new(errors).into());
        }

        resolve::labels(gh, target, &current, &mut dir).await?;
        let (rulesets, undeclared) = diff::rulesets(&desired, &current, prune, &dir);
        // Undeclared rulesets kept without --prune count toward the limit as well. validate has
        // already checked the declared ones alone
        if !undeclared.is_empty() && desired.len() + undeclared.len() > MAX_RULESETS {
            return Err(Diagnostics::new(vec![
                Diagnostic::error(
                    Code::TooManyRulesets,
                    format!(
                        "at most {MAX_RULESETS} rulesets are allowed per repository ({} declared \
                        and {} kept because they are not declared)",
                        desired.len(),
                        undeclared.len()
                    ),
                )
                .at("ruleset")
                .help("delete the undeclared rulesets with --prune, or declare fewer rulesets"),
            ])
            .into());
        }
        let mut ruleset_steps = Vec::new();
        for change in &rulesets {
            let name = change.name.clone().unwrap_or_default();
            let want = desired.iter().find(|d| d.name == name);
            let have = current.iter().find(|c| c.name == name);
            let step = match (change.action, want, have.and_then(|h| h.id)) {
                (Action::Create, Some(want), _) => Step::CreateRuleset {
                    name,
                    body: want.to_api(),
                },
                (Action::Update, Some(want), Some(id)) => Step::UpdateRuleset {
                    name,
                    id,
                    body: want.to_api(),
                },
                (Action::Delete, _, Some(id)) => Step::DeleteRuleset { name, id },
                (Action::NoChange, ..) => continue,
                _ => bail!("cannot build the apply steps for ruleset \"{name}\""),
            };
            ruleset_steps.push(step);
        }
        order_ruleset_steps(&mut ruleset_steps, current.len());
        steps.extend(ruleset_steps);
        changes.extend(rulesets);
        ignored.extend(undeclared.iter().map(|name| format!("ruleset \"{name}\"")));
    }

    let count = |action| changes.iter().filter(|c| c.action == action).count();
    let summary = Summary {
        add: count(Action::Create),
        change: count(Action::Update),
        destroy: count(Action::Delete),
    };

    Ok(Plan {
        repository: target.to_string(),
        changes,
        ignored,
        undeclared,
        warnings,
        summary,
        writes: steps.iter().map(Step::writes).sum(),
        steps,
    })
}

/// Creating before deleting would pass through more rulesets than the limit allows (for example,
/// when one is renamed at the limit with --prune), so delete first then. Otherwise creations go
/// first, so a failure leaves the old protection in place.
fn order_ruleset_steps(steps: &mut [Step], current: usize) {
    let creates = steps
        .iter()
        .filter(|s| matches!(s, Step::CreateRuleset { .. }))
        .count();
    if current + creates > MAX_RULESETS {
        steps.sort_by_key(|s| !matches!(s, Step::DeleteRuleset { .. }));
    }
}

fn repository_step(config: &Config, change: &ResourceChange) -> Option<Step> {
    if change.action != Action::Update {
        return None;
    }
    let declared = serde_json::to_value(&config.repository).ok()?;
    let mut fields = Map::new();
    let mut topics = None;
    for field in change.fields.iter().filter(|f| f.op != Op::Same) {
        if field.path == "topics" {
            topics = Some(json!({ "names": declared["topics"] }));
        } else {
            fields.insert(field.path.clone(), declared[&field.path].clone());
        }
    }
    Some(Step::Repository { fields, topics })
}

/// Fetch the repository's own rulesets. Rulesets inherited from the Organization cannot be
/// changed on the repository side, so they are not included.
async fn current_rulesets(
    gh: &mut GitHub,
    target: &RepoRef,
    warnings: &mut Vec<String>,
) -> Result<Vec<RulesetState>> {
    let list = gh
        .get_all(&format!("/repos/{target}/rulesets?includes_parents=false"))
        .await?;

    let mut out = Vec::new();
    for entry in &list {
        let Some(id) = entry["id"].as_u64() else {
            bail!("ruleset list entry has no id");
        };
        let detail = gh.get(&format!("/repos/{target}/rulesets/{id}")).await?;
        let state = RulesetState::from_api(&detail)?;
        for key in state.extra_conditions.keys() {
            warnings.push(format!(
                "ruleset \"{}\": keeping unsupported condition \"{key}\" at its current value",
                state.name
            ));
        }
        for actor in &state.actors {
            if actor_type_to_toml(&actor.actor_type).is_none() {
                warnings.push(format!(
                    "ruleset \"{}\": keeping unsupported bypass actor type \"{}\" at its current \
                    value",
                    state.name, actor.actor_type
                ));
            }
        }
        for rule in state.rules.keys() {
            if !Rules::KEYS.contains(&rule.as_str()) {
                warnings.push(format!(
                    "ruleset \"{}\": keeping unsupported rule \"{rule}\" at its current value",
                    state.name
                ));
            }
        }
        out.push(state);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .map(|s| match s {
                Step::CreateRuleset { .. } => "create",
                Step::UpdateRuleset { .. } => "update",
                Step::DeleteRuleset { .. } => "delete",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn deletes_rulesets_first_only_at_the_limit() {
        let steps = || {
            vec![
                Step::CreateRuleset {
                    name: "new".to_string(),
                    body: json!({}),
                },
                Step::UpdateRuleset {
                    name: "kept".to_string(),
                    id: 1,
                    body: json!({}),
                },
                Step::DeleteRuleset {
                    name: "old".to_string(),
                    id: 2,
                },
            ]
        };

        let mut below = steps();
        order_ruleset_steps(&mut below, MAX_RULESETS - 1);
        assert_eq!(kinds(&below), ["create", "update", "delete"]);

        let mut at = steps();
        order_ruleset_steps(&mut at, MAX_RULESETS);
        assert_eq!(kinds(&at), ["delete", "create", "update"]);
    }
}
