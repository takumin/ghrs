//! Read the current state from the GitHub API and convert it into the `.ghrs.toml` model.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value as Json, json};
use std::collections::HashMap;

use crate::access;
use crate::auth::SelfApp;
use crate::diagnostic::Diagnostics;
use crate::github::GitHub;
use crate::model::{
    BypassActor, Collaborator, Conditions, Config, Meta, Repository, Rules, Ruleset, Team,
};
use crate::repo::RepoRef;
use crate::schema::{for_each_review_actor, review_actor_type_to_toml, role_name};
use crate::validate;

pub struct Importer {
    pub gh: GitHub,
    app_names: HashMap<u64, String>,
    /// For an Organization-owned repository, that Organization's team ID -> slug
    team_names: Option<HashMap<u64, String>>,
    pub warnings: Vec<String>,
}

impl Importer {
    /// When running as an App, the App itself is output as `self`.
    pub fn new(gh: GitHub, self_app: Option<&SelfApp>) -> Self {
        Self {
            gh,
            app_names: self_app
                .map(|app| HashMap::from([(app.id, "self".to_string())]))
                .unwrap_or_default(),
            team_names: None,
            warnings: Vec::new(),
        }
    }

    async fn get(&mut self, route: String) -> Result<Json> {
        self.gh.get(&route).await
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    pub async fn run(&mut self, owner: &str, repo: &str) -> Result<Config> {
        let (repository, owned_by_org) = self.repository(owner, repo).await?;
        if owned_by_org {
            self.team_names = Some(self.org_teams(owner).await?);
        }
        let rulesets = self.rulesets(owner, repo).await?;
        // Outputting it would fail validation in plan anyway, so stop here and prompt for removal
        if !owned_by_org {
            for ruleset in &rulesets {
                let legacy = ruleset
                    .bypass_actors
                    .iter()
                    .any(|a| a.actor_type == "organization_admin");
                if legacy {
                    let diagnostic = validate::organization_admin_on_personal(&ruleset.name).help(
                        "it is left over from older GitHub behavior and has no effect; \
                            delete it in the web UI, then run import again",
                    );
                    return Err(Diagnostics::new(vec![diagnostic]).into());
                }
            }
        }

        let target = RepoRef {
            owner: owner.to_string(),
            name: repo.to_string(),
        };
        let mut collaborators = Vec::new();
        for grant in access::current_collaborators(&mut self.gh, &target).await? {
            if grant.invitation.is_some() {
                self.warn(format!(
                    "collaborator \"{}\" has not accepted the invitation",
                    grant.key
                ));
            }
            collaborators.push(Collaborator {
                login: grant.key,
                permission: grant.permission,
            });
        }
        let mut teams = Vec::new();
        if owned_by_org {
            for grant in access::current_teams(&mut self.gh, &target).await? {
                teams.push(Team {
                    slug: grant.key,
                    permission: grant.permission,
                });
            }
        }

        Ok(Config {
            meta: Meta {
                owner: Some(owner.to_string()),
                repo: Some(repo.to_string()),
            },
            repository,
            rulesets,
            collaborators,
            teams,
        })
    }

    /// Repository settings, and whether the repository is Organization-owned.
    async fn repository(&mut self, owner: &str, repo: &str) -> Result<(Repository, bool)> {
        let v = self.get(format!("/repos/{owner}/{repo}")).await?;
        let owned_by_org = v["owner"]["type"] == "Organization";

        // Omission means "not managed", so output everything even if equal to the default
        let repository = Repository {
            description: Some(string_or_empty(&v["description"])),
            homepage: Some(string_or_empty(&v["homepage"])),
            visibility: Some(string_or_empty(&v["visibility"])),
            has_issues: Some(v["has_issues"].as_bool().unwrap_or(false)),
            has_wiki: Some(v["has_wiki"].as_bool().unwrap_or(false)),
            has_projects: Some(v["has_projects"].as_bool().unwrap_or(false)),
            has_discussions: Some(v["has_discussions"].as_bool().unwrap_or(false)),
            allow_merge_commit: Some(v["allow_merge_commit"].as_bool().unwrap_or(false)),
            allow_squash_merge: Some(v["allow_squash_merge"].as_bool().unwrap_or(false)),
            allow_rebase_merge: Some(v["allow_rebase_merge"].as_bool().unwrap_or(false)),
            allow_auto_merge: Some(v["allow_auto_merge"].as_bool().unwrap_or(false)),
            delete_branch_on_merge: Some(v["delete_branch_on_merge"].as_bool().unwrap_or(false)),
            default_branch: Some(string_or_empty(&v["default_branch"])),
            topics: Some(string_array(&v["topics"])),
        };
        Ok((repository, owned_by_org))
    }

    /// The repository's own rulesets. Rulesets inherited from the Organization are not managed by
    /// plan, so they are not output (if output, plan would try to create them).
    async fn rulesets(&mut self, owner: &str, repo: &str) -> Result<Vec<Ruleset>> {
        let list = self
            .gh
            .get_all(&format!(
                "/repos/{owner}/{repo}/rulesets?includes_parents=false"
            ))
            .await?;

        let mut out = Vec::new();
        for entry in list {
            let id = entry["id"].as_u64().context("ruleset has no id")?;
            let detail = self
                .get(format!("/repos/{owner}/{repo}/rulesets/{id}"))
                .await?;
            out.push(self.ruleset(&detail).await?);
        }
        Ok(out)
    }

    async fn ruleset(&mut self, v: &Json) -> Result<Ruleset> {
        let name = string_or_empty(&v["name"]);
        let target = string_or_empty(&v["target"]);

        // Only conditions.ref_name is handled. The repository_name / repository_id conditions
        // used by Organization rulesets are out of scope, but warn so they are not silently
        // dropped.
        let conditions = &v["conditions"];
        if let Some(obj) = conditions.as_object() {
            for key in obj.keys() {
                if key != "ref_name" {
                    self.warn(format!(
                        "ruleset \"{name}\": ignored unsupported condition \"{key}\""
                    ));
                }
            }
        }
        let ref_name = &conditions["ref_name"];

        crate::state::require_bypass_actors(v, &name)?;
        let mut bypass_actors = Vec::new();
        if let Some(actors) = v["bypass_actors"].as_array() {
            for actor in actors.clone() {
                bypass_actors.push(self.bypass_actor(&name, &actor).await?);
            }
        }

        let rules = self.rules(&name, &target, &v["rules"]).await?;

        Ok(Ruleset {
            name,
            target,
            enforcement: string_or_empty(&v["enforcement"]),
            conditions: Conditions {
                include: string_array(&ref_name["include"]),
                exclude: string_array(&ref_name["exclude"]),
            },
            bypass_actors,
            rules,
        })
    }

    async fn bypass_actor(&mut self, ruleset: &str, v: &Json) -> Result<BypassActor> {
        let actor_type = string_or_empty(&v["actor_type"]);
        let actor_id = v["actor_id"].as_u64();
        let mode = string_or_empty(&v["bypass_mode"]);

        let (toml_type, name) = match actor_type.as_str() {
            "RepositoryRole" => {
                let id = actor_id.context("RepositoryRole has no actor_id")?;
                let name = match role_name(id) {
                    Some(name) => name.to_string(),
                    None => {
                        self.warn(format!(
                            "ruleset \"{ruleset}\": observed unknown repository role ID {id}; \
                            review the table of built-in roles"
                        ));
                        id.to_string()
                    }
                };
                ("repository_role", Some(name))
            }
            "Integration" => {
                let id = actor_id.context("Integration has no actor_id")?;
                let name = self.app_slug(id, &format!("ruleset \"{ruleset}\"")).await;
                ("integration", Some(name))
            }
            "OrganizationAdmin" => ("organization_admin", None),
            "Team" => {
                let id = actor_id.context("Team has no actor_id")?;
                let slug = self.team_names.as_ref().and_then(|t| t.get(&id)).cloned();
                let name = match slug {
                    Some(slug) => slug,
                    None => {
                        self.warn(format!(
                            "ruleset \"{ruleset}\": cannot resolve team ID {id} to a name; wrote \
                            the numeric ID instead"
                        ));
                        id.to_string()
                    }
                };
                ("team", Some(name))
            }
            // A deploy key refers to the whole kind, and actor_id is null (observed)
            "DeployKey" => ("deploy_key", None),
            other => bail!("ruleset \"{ruleset}\": unknown actor_type \"{other}\""),
        };

        Ok(BypassActor {
            actor_type: toml_type.to_string(),
            name,
            mode,
        })
    }

    async fn org_teams(&mut self, org: &str) -> Result<HashMap<u64, String>> {
        let list = self.gh.get_all(&format!("/orgs/{org}/teams")).await?;
        Ok(list
            .iter()
            .filter_map(|t| Some((t["id"].as_u64()?, t["slug"].as_str()?.to_string())))
            .collect())
    }

    /// `GET /app/{id}` returns public Apps and private Apps owned by the token's owner.
    /// Someone else's private App returns 404, and there is no way to map the ID to a name
    /// (`/user/installations` requires the App's user-to-server token, and
    /// `/orgs/{org}/installations` does not exist for personal accounts). When it cannot be
    /// resolved, output the numeric ID as-is and warn.
    async fn app_slug(&mut self, id: u64, context: &str) -> String {
        if let Some(slug) = self.app_names.get(&id) {
            return slug.clone();
        }
        let slug = match self.get(format!("/app/{id}")).await {
            Ok(v) => string_or_empty(&v["slug"]),
            Err(_) => {
                self.warn(format!(
                    "{context}: cannot resolve App ID {id} to a name (probably a private App); \
                    wrote the numeric ID instead"
                ));
                id.to_string()
            }
        };
        self.app_names.insert(id, slug.clone());
        slug
    }

    /// Fold the rule array into a table.
    ///
    /// Rules without parameters become booleans; rules with parameters become tables.
    ///
    /// Rules and parameters [`Rules`] does not know are left out with a warning. plan keeps them
    /// at their current values, so leaving them out does not change them.
    async fn rules(&mut self, ruleset: &str, target: &str, v: &Json) -> Result<Rules> {
        let mut out = Map::new();
        for rule in v.as_array().into_iter().flatten() {
            let rule_type = string_or_empty(&rule["type"]);
            if !Rules::KEYS.contains(&rule_type.as_str()) {
                self.warn(format!(
                    "ruleset \"{ruleset}\": rule \"{rule_type}\" is not supported; left it out \
                    of the declaration, and plan keeps it as is"
                ));
                continue;
            }
            let params = rule.get("parameters").filter(|p| !p.is_null());

            let value = match (rule_type.as_str(), params) {
                (_, None) => Json::Bool(true),
                // `update = true` is sugar for update_allows_fetch_and_merge = false.
                ("update", Some(params))
                    if params["update_allows_fetch_and_merge"] == Json::Bool(false) =>
                {
                    Json::Bool(true)
                }
                ("required_status_checks", Some(params)) => {
                    self.status_checks(ruleset, params).await
                }
                ("pull_request", Some(params)) => {
                    let params = self.named_review_actors(ruleset, params).await;
                    self.plain_params(ruleset, &rule_type, target, &params)?
                }
                (_, Some(params)) => self.plain_params(ruleset, &rule_type, target, params)?,
            };
            out.insert(rule_type, value);
        }
        serde_json::from_value(Json::Object(out))
            .with_context(|| format!("ruleset \"{ruleset}\": cannot read the rules"))
    }

    /// Rewrite the `{id, type}` of `pull_request` reviewers and dismissal_restrictions into the
    /// declaration's `{type, name}`. IDs that cannot be mapped back to names are
    /// left numeric with a warning. plan rejects them as declaration errors.
    async fn named_review_actors(&mut self, ruleset: &str, params: &Json) -> Json {
        let mut params = params.clone();
        let mut ids = Vec::new();
        let _ = for_each_review_actor(&mut params, |actor| {
            if let (Some(kind), Some(id)) = (actor["type"].as_str(), actor["id"].as_u64()) {
                ids.push((kind.to_string(), id));
            }
            Ok::<(), ()>(())
        });

        let mut names = HashMap::new();
        for (kind, id) in ids {
            let name = match kind.as_str() {
                "Team" => self.team_names.as_ref().and_then(|t| t.get(&id)).cloned(),
                "RepositoryRole" => role_name(id).map(str::to_string),
                "User" => match self.get(format!("/user/{id}")).await {
                    Ok(v) => v["login"].as_str().map(str::to_string),
                    Err(_) => None,
                },
                _ => None,
            };
            let name = name.unwrap_or_else(|| {
                self.warn(format!(
                    "ruleset \"{ruleset}\": cannot resolve {kind} ID {id} in pull_request to a \
                    name; wrote the numeric ID instead"
                ));
                id.to_string()
            });
            names.insert((kind, id), name);
        }

        let _ = for_each_review_actor(&mut params, |actor| {
            let api = actor["type"].as_str().unwrap_or_default().to_string();
            let id = actor["id"].as_u64().unwrap_or_default();
            let kind = review_actor_type_to_toml(&api).unwrap_or(&api);
            let name = names.get(&(api.clone(), id)).cloned().unwrap_or_default();
            *actor = serde_json::json!({ "type": kind, "name": name });
            Ok::<(), ()>(())
        });
        params
    }

    async fn status_checks(&mut self, ruleset: &str, params: &Json) -> Json {
        let mut checks = Vec::new();
        for check in params["required_status_checks"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let mut entry = Map::new();
            entry.insert("context".to_string(), check["context"].clone());
            if let Some(id) = check["integration_id"].as_u64() {
                let name = self
                    .app_slug(id, &format!("ruleset \"{ruleset}\" status check"))
                    .await;
                entry.insert("integration".to_string(), Json::String(name));
            }
            checks.push(Json::Object(entry));
        }
        let flag = |key: &str| params[key].as_bool().unwrap_or(false);
        json!({
            "strict": flag("strict_required_status_checks_policy"),
            "do_not_enforce_on_create": flag("do_not_enforce_on_create"),
            "checks": checks,
        })
    }

    fn plain_params(
        &mut self,
        ruleset: &str,
        rule_type: &str,
        target: &str,
        params: &Json,
    ) -> Result<Json> {
        let obj = params.as_object().with_context(|| {
            format!("ruleset \"{ruleset}\": parameters of rule \"{rule_type}\" are not an object")
        })?;
        let known = Rules::param_keys(rule_type).unwrap_or_default();

        let mut out = Map::new();
        for (key, value) in obj {
            if key.ends_with("_id") || key.ends_with("_ids") {
                bail!(
                    "ruleset \"{ruleset}\": parameter \"{key}\" of rule \"{rule_type}\" is a \
                    numeric ID; \
                    it cannot be imported until name resolution for it is implemented"
                );
            }
            if !known.contains(&key.as_str()) {
                self.warn(format!(
                    "ruleset \"{ruleset}\": parameter \"{key}\" of rule \"{rule_type}\" is not \
                    supported; left it out of the declaration, and plan keeps it as is"
                ));
                continue;
            }
            out.insert(key.clone(), value.clone());
        }

        if rule_type == "pull_request" && target != "branch" {
            self.warn(format!(
                "ruleset \"{ruleset}\": pull_request rule is attached to target = \"{target}\""
            ));
        }

        Ok(Json::Object(out))
    }
}

/// Renders the imported declaration. Arrays of tables (`bypass_actors`, `checks`, ...) are written
/// as inline arrays, one element per line, so that every table header names the ruleset it
/// belongs to instead of repeating `[[ruleset.main.bypass_actors]]` per element.
pub fn render(config: &Config) -> Result<String> {
    let text = toml::to_string_pretty(config)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    inline_arrays_of_tables(doc.as_table_mut());
    Ok(doc.to_string())
}

fn inline_arrays_of_tables(table: &mut toml_edit::Table) {
    for (mut key, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Table(child) => inline_arrays_of_tables(child),
            toml_edit::Item::ArrayOfTables(tables) => {
                let mut array = toml_edit::Array::new();
                for table in std::mem::take(tables) {
                    let mut value = toml_edit::Value::InlineTable(table.into_inline_table());
                    value.decor_mut().set_prefix("\n  ");
                    array.push_formatted(value);
                }
                array.set_trailing("\n");
                array.set_trailing_comma(true);
                *item = toml_edit::value(array);
                // The key of an array of tables has no decor of its own, unlike `key = value`
                key.leaf_decor_mut().set_suffix(" ");
            }
            _ => {}
        }
    }
}

fn string_or_empty(v: &Json) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn string_array(v: &Json) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(string_or_empty).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_keyed_tables_with_inline_arrays() {
        let config: Config = toml::from_str(
            r#"
[ruleset.main]
target = "branch"
enforcement = "active"

[[ruleset.main.bypass_actors]]
type = "repository_role"
name = "admin"

[[ruleset.main.bypass_actors]]
type = "deploy_key"

[ruleset.main.rules.required_status_checks]
strict = true
checks = [{ context = "CI", integration = "github-actions" }]

[ruleset.main.rules.pull_request]
required_reviewers = [
    { file_patterns = ["*"], minimum_approvals = 1, reviewer = { type = "team", name = "t" } },
]

[collaborator.octocat]
permission = "push"
"#,
        )
        .unwrap();
        let text = render(&config).unwrap();
        assert!(!text.contains("[["), "{text}");
        assert!(
            text.contains(
                "bypass_actors = [\n  \
                { type = \"repository_role\", name = \"admin\", mode = \"always\" },\n  \
                { type = \"deploy_key\", mode = \"always\" },\n]"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "required_reviewers = [\n  \
                { file_patterns = [\"*\"], minimum_approvals = 1, \
                reviewer = { type = \"team\", name = \"t\" } },\n]"
            ),
            "{text}"
        );
        assert!(
            text.contains("[collaborator.octocat]\npermission = \"push\""),
            "{text}"
        );

        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.rulesets[0].name, "main");
        assert_eq!(back.rulesets[0].bypass_actors.len(), 2);
        assert_eq!(back.collaborators[0].login, "octocat");
    }
}
