//! Name resolution. Converts names in the declaration to numeric API IDs.
//!
//! Resolution results are gathered into [`Directory`], which diff computation only reads. Only
//! functions in this module call the API, so diff computation can be tested as pure functions.

use anyhow::{Result, bail};
use std::collections::HashMap;

use crate::auth::SelfApp;
use crate::github::GitHub;
use crate::model::{Config, Rule};
use crate::repo::RepoRef;
use crate::schema::role_name;
use crate::state::RulesetState;

/// Mapping between names and IDs. The same table is used in both directions.
#[derive(Default)]
pub struct Directory {
    apps: HashMap<String, u64>,
    app_names: HashMap<u64, String>,
    teams: HashMap<String, u64>,
    team_names: HashMap<u64, String>,
    users: HashMap<String, u64>,
    user_names: HashMap<u64, String>,
    /// App slug -> installation ID, for `pull_request` actors
    installations: HashMap<String, u64>,
    installation_names: HashMap<u64, String>,
}

impl Directory {
    pub fn app_id(&self, name: &str) -> Option<u64> {
        self.apps.get(name).copied()
    }

    pub fn team_id(&self, slug: &str) -> Option<u64> {
        self.teams.get(slug).copied()
    }

    pub fn installation_id(&self, slug: &str) -> Option<u64> {
        self.installations.get(slug).copied()
    }

    /// login is case-insensitive
    pub fn user_id(&self, login: &str) -> Option<u64> {
        self.users.get(&login.to_ascii_lowercase()).copied()
    }

    /// Convert an App ID back to the form written in `.ghrs.toml`. Private Apps that cannot be
    /// resolved stay numeric.
    pub fn app_name(&self, id: u64) -> String {
        self.app_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// Return a bypass actor's `name` in the form written in `.ghrs.toml`.
    pub fn actor_name(&self, actor_type: &str, actor_id: Option<u64>) -> Option<String> {
        let id = actor_id?;
        Some(match actor_type {
            "Integration" => self.app_name(id),
            "RepositoryRole" => role_name(id).map_or_else(|| id.to_string(), str::to_string),
            "Team" => self
                .team_names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| id.to_string()),
            "User" => self
                .user_names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| id.to_string()),
            "IntegrationInstallation" => self
                .installation_names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| id.to_string()),
            _ => return None,
        })
    }

    /// When running as an App, bind the reserved word `self` to the App's own ID and
    /// installation ID. If the App itself appears in the current state, it is displayed as `self`.
    pub fn add_self(&mut self, app: &SelfApp) {
        self.add_app("self", app.id);
        self.add_installation("self", app.installation_id);
    }

    fn add_app(&mut self, name: &str, id: u64) {
        self.apps.insert(name.to_string(), id);
        if name != id.to_string() {
            self.app_names.insert(id, name.to_string());
        }
    }

    fn add_team(&mut self, slug: &str, id: u64) {
        self.teams.insert(slug.to_string(), id);
        self.team_names.insert(id, slug.to_string());
    }

    fn add_user(&mut self, login: &str, id: u64) {
        self.users.insert(login.to_ascii_lowercase(), id);
        self.user_names.insert(id, login.to_string());
    }

    fn add_installation(&mut self, slug: &str, id: u64) {
        self.installations.insert(slug.to_string(), id);
        // Keep `self` as the display name when the org listing also names the App
        self.installation_names
            .entry(id)
            .or_insert_with(|| slug.to_string());
    }

    #[cfg(test)]
    pub fn with_app(mut self, name: &str, id: u64) -> Self {
        self.add_app(name, id);
        self
    }

    #[cfg(test)]
    pub fn with_team(mut self, slug: &str, id: u64) -> Self {
        self.add_team(slug, id);
        self
    }

    #[cfg(test)]
    pub fn with_user(mut self, login: &str, id: u64) -> Self {
        self.add_user(login, id);
        self
    }

    #[cfg(test)]
    pub fn with_installation(mut self, slug: &str, id: u64) -> Self {
        self.add_installation(slug, id);
        self
    }
}

/// Resolve every name in the declaration file to an ID. Errors if even one cannot be resolved.
pub async fn declared(
    gh: &mut GitHub,
    target: &RepoRef,
    config: &Config,
    dir: &mut Directory,
) -> Result<()> {
    for ruleset in &config.rulesets {
        let at = format!("ruleset \"{}\"", ruleset.name);

        for actor in &ruleset.bypass_actors {
            let Some(name) = actor.name.as_deref() else {
                continue;
            };
            match actor.actor_type.as_str() {
                "integration" => app(gh, dir, name, &at, "integration").await?,
                "team" => team(gh, dir, &target.owner, name, &at).await?,
                _ => {}
            }
        }

        // pull_request reviewers and dismissal_restriction actors
        if let Some(Rule::Params(pr)) = &ruleset.rules.pull_request {
            for (_, actor) in pr.review_actors() {
                match actor.actor_type.as_str() {
                    "team" => team(gh, dir, &target.owner, &actor.name, &at).await?,
                    "user" => user(gh, dir, &actor.name, &at).await?,
                    "integration" => {
                        installation(gh, dir, &target.owner, &actor.name, &at).await?;
                    }
                    _ => {}
                }
            }
        }

        if let Some(Rule::Params(checks)) = &ruleset.rules.required_status_checks {
            for check in checks.checks.iter().flatten() {
                if let Some(name) = &check.integration {
                    app(gh, dir, name, &at, "status check integration").await?;
                }
            }
        }
    }
    Ok(())
}

/// Map IDs that appear only in the current state back to names for display. On failure they are
/// just shown as numbers, not treated as an error (other users' private Apps can never be
/// looked up).
pub async fn labels(
    gh: &mut GitHub,
    target: &RepoRef,
    current: &[RulesetState],
    dir: &mut Directory,
) -> Result<()> {
    let mut app_ids = Vec::new();
    let mut team_ids = Vec::new();
    let mut user_ids = Vec::new();
    let mut installation_ids = Vec::new();
    for state in current {
        for actor in &state.actors {
            match (actor.actor_type.as_str(), actor.actor_id) {
                ("Integration", Some(id)) => app_ids.push(id),
                ("Team", Some(id)) => team_ids.push(id),
                _ => {}
            }
        }
        app_ids.extend(state.check_integrations());
        for (kind, id) in state.review_actors() {
            match kind.as_str() {
                "Team" => team_ids.push(id),
                "User" => user_ids.push(id),
                "IntegrationInstallation" => installation_ids.push(id),
                _ => {}
            }
        }
    }

    // Only an Organization owner can list installations. Without the permission they are just
    // shown as numbers
    if installation_ids
        .iter()
        .any(|id| !dir.installation_names.contains_key(id))
        && let Some(list) = org_installations(gh, &target.owner).await?
    {
        for (id, slug) in list {
            dir.add_installation(&slug, id);
        }
    }

    // There is no standalone way to look up a slug from a team ID, so fetch the list only
    // when there are unknown IDs
    if team_ids.iter().any(|id| !dir.team_names.contains_key(id)) {
        for t in gh.get_all(&format!("/orgs/{}/teams", target.owner)).await? {
            if let (Some(id), Some(slug)) = (t["id"].as_u64(), t["slug"].as_str()) {
                dir.add_team(slug, id);
            }
        }
    }
    user_ids.sort_unstable();
    user_ids.dedup();
    for id in user_ids {
        if dir.user_names.contains_key(&id) {
            continue;
        }
        if let Some(v) = gh.get_optional(&format!("/user/{id}")).await?
            && let Some(login) = v["login"].as_str()
        {
            dir.add_user(login, id);
        }
    }

    app_ids.sort_unstable();
    app_ids.dedup();
    for id in app_ids {
        if dir.app_names.contains_key(&id) {
            continue;
        }
        if let Some(v) = gh.get_optional(&format!("/app/{id}")).await?
            && let Some(slug) = v["slug"].as_str()
        {
            dir.app_names.insert(id, slug.to_string());
        }
    }
    Ok(())
}

async fn app(gh: &mut GitHub, dir: &mut Directory, name: &str, at: &str, what: &str) -> Result<()> {
    if dir.apps.contains_key(name) {
        return Ok(());
    }
    if name == "self" {
        bail!(
            "{at}: {what} \"self\" is only available with GitHub App authentication (it cannot be \
            resolved with a PAT)"
        );
    }
    // Other users' private Apps can only be written as numeric IDs
    if let Ok(id) = name.parse::<u64>() {
        dir.add_app(name, id);
        return Ok(());
    }
    match gh.get_optional(&format!("/apps/{name}")).await? {
        Some(v) => match v["id"].as_u64() {
            Some(id) => {
                dir.add_app(name, id);
                Ok(())
            }
            None => bail!("response of GET /apps/{name} has no id"),
        },
        None => bail!(
            "cannot resolve bypass actor\n  \
            {what} \"{name}\" in {at}\n  \
            -> the App does not exist, or it is a private App owned by someone else\n  \
            -> a private App can only be resolved with its owner's token; a numeric ID also works"
        ),
    }
}

async fn team(gh: &mut GitHub, dir: &mut Directory, org: &str, slug: &str, at: &str) -> Result<()> {
    if dir.teams.contains_key(slug) {
        return Ok(());
    }
    let Some(v) = gh
        .get_optional(&format!("/orgs/{org}/teams/{slug}"))
        .await?
    else {
        // closed / secret teams are visible only to Organization members. Even a token of an
        // outside collaborator with admin permission gets 404 (observed)
        bail!(
            "{at}: team \"{slug}\" not found in Organization \"{org}\"\n  \
            -> the team does not exist, or the token owner is not a member of the Organization\n  \
            -> resolving team names needs a token of an Organization member with read:org"
        );
    };
    let Some(id) = v["id"].as_u64() else {
        bail!("response of GET /orgs/{org}/teams/{slug} has no id");
    };
    dir.add_team(slug, id);
    Ok(())
}

/// The Organization's App installations as (installation ID, App slug). `None` when the token
/// cannot list them.
///
/// The documentation asks classic PATs for `admin:read`, but an owner's token with only
/// `read:org` can list them, and a non-member gets 404 (observed).
pub async fn org_installations(gh: &mut GitHub, org: &str) -> Result<Option<Vec<(u64, String)>>> {
    let Some(list) = gh
        .get_all_wrapped(&format!("/orgs/{org}/installations"), "installations")
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(
        list.iter()
            .filter_map(|i| Some((i["id"].as_u64()?, i["app_slug"].as_str()?.to_string())))
            .collect(),
    ))
}

/// Resolve an App in `pull_request` to its installation ID in the Organization.
async fn installation(
    gh: &mut GitHub,
    dir: &mut Directory,
    org: &str,
    slug: &str,
    at: &str,
) -> Result<()> {
    if dir.installations.contains_key(slug) {
        return Ok(());
    }
    if slug == "self" {
        bail!(
            "{at}: integration \"self\" in pull_request is only available with GitHub App \
            authentication (it cannot be resolved with a PAT)"
        );
    }
    let Some(list) = org_installations(gh, org).await? else {
        bail!(
            "{at}: cannot list the App installations of Organization \"{org}\" to resolve \
            integration \"{slug}\" in pull_request\n  \
            -> resolving Apps in pull_request needs a token of an owner of the Organization"
        );
    };
    for (id, name) in list {
        dir.add_installation(&name, id);
    }
    if !dir.installations.contains_key(slug) {
        bail!(
            "{at}: App \"{slug}\" in pull_request is not installed in Organization \"{org}\"\n  \
            -> install the App on the Organization, or check the App slug"
        );
    }
    Ok(())
}

async fn user(gh: &mut GitHub, dir: &mut Directory, login: &str, at: &str) -> Result<()> {
    if dir.user_id(login).is_some() {
        return Ok(());
    }
    let Some(v) = gh.get_optional(&format!("/users/{login}")).await? else {
        bail!("{at}: user \"{login}\" not found");
    };
    let Some(id) = v["id"].as_u64() else {
        bail!("response of GET /users/{login} has no id");
    };
    dir.add_user(login, id);
    Ok(())
}
