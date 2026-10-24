//! Normalized representation of a ruleset. Both the current state and the declaration are
//! converted to this shape before comparison.
//!
//! Comparison uses the API's vocabulary (numeric IDs, API parameter names). Comparing in the
//! name space would make private Apps, which cannot be mapped back to names, always show up as
//! a diff. Display names are looked up from [`Directory`] at the diff computation stage.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeMap;

use crate::diagnostic::{Code, Diagnostic, Diagnostics};
use crate::model::{Rules, Ruleset};
use crate::resolve::Directory;
use crate::schema::{
    ACTORS_WITHOUT_ID, PARAMETERIZED_RULES, REQUIRED_PARAMS, actor_type_to_api, actor_type_to_toml,
    for_each_review_actor, review_actor_type_to_api, role_id, rule_defaults, rule_to_api,
};
use crate::validate;

pub struct RulesetState {
    pub id: Option<u64>,
    pub name: String,
    pub target: String,
    pub enforcement: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Conditions other than `ref_name`. ghrs does not handle them, but keeps them so the
    /// full-replacement PUT does not erase them.
    pub extra_conditions: Map<String, Json>,
    pub actors: Vec<Actor>,
    /// Rule type -> parameters. `None` for rules without parameters.
    pub rules: BTreeMap<String, Option<Json>>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Actor {
    /// The API's `actor_type` (e.g. `Integration`)
    pub actor_type: String,
    pub actor_id: Option<u64>,
    pub mode: String,
}

impl RulesetState {
    /// Built from the response of `GET /repos/{o}/{r}/rulesets/{id}`. Of the read-only fields,
    /// only `id` is picked up, as the target of the update request.
    pub fn from_api(v: &Json) -> Result<Self> {
        let name = str_field(v, "name")?;

        let mut extra_conditions = v["conditions"].as_object().cloned().unwrap_or_default();
        let ref_name = extra_conditions.remove("ref_name").unwrap_or(Json::Null);

        require_bypass_actors(v, &name)?;
        let actors = v["bypass_actors"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| {
                Ok(Actor {
                    actor_type: str_field(a, "actor_type")?,
                    actor_id: a["actor_id"].as_u64(),
                    mode: str_field(a, "bypass_mode")?,
                })
            })
            .collect::<Result<_>>()
            .with_context(|| format!("bypass_actors of ruleset \"{name}\""))?;

        let mut rules = BTreeMap::new();
        for rule in v["rules"].as_array().into_iter().flatten() {
            let params = rule.get("parameters").filter(|p| !p.is_null()).cloned();
            rules.insert(str_field(rule, "type")?, params);
        }

        Ok(Self {
            id: v["id"].as_u64(),
            target: str_field(v, "target")?,
            enforcement: str_field(v, "enforcement")?,
            include: strings(&ref_name["include"]),
            exclude: strings(&ref_name["exclude"]),
            extra_conditions,
            actors,
            rules: normalize(rules),
            name,
        })
    }

    /// Body sent to `POST` / `PUT /repos/{o}/{r}/rulesets`.
    ///
    /// PUT is a full replacement, so always build a complete object. Read-only fields
    /// (`id` / `source`, etc.) are not included.
    pub fn to_api(&self) -> Json {
        let mut conditions = self.extra_conditions.clone();
        conditions.insert(
            "ref_name".to_string(),
            json!({ "include": self.include, "exclude": self.exclude }),
        );

        let actors: Vec<Json> = self
            .actors
            .iter()
            .map(|a| {
                json!({
                    "actor_id": a.actor_id,
                    "actor_type": a.actor_type,
                    "bypass_mode": a.mode,
                })
            })
            .collect();

        let rules: Vec<Json> = self
            .rules
            .iter()
            .map(|(rule, params)| match params {
                Some(params) => json!({ "type": rule, "parameters": params }),
                None => json!({ "type": rule }),
            })
            .collect();

        json!({
            "name": self.name,
            "target": self.target,
            "enforcement": self.enforcement,
            "conditions": conditions,
            "bypass_actors": actors,
            "rules": rules,
        })
    }

    /// (API type, ID) of actors appearing in `pull_request` reviewers and
    /// dismissal_restriction.
    pub fn review_actors(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        if let Some(Some(params)) = self.rules.get("pull_request") {
            let mut params = params.clone();
            let _ = for_each_review_actor(&mut params, |actor| {
                if let (Some(kind), Some(id)) = (actor["type"].as_str(), actor["id"].as_u64()) {
                    out.push((kind.to_string(), id));
                }
                Ok::<(), ()>(())
            });
        }
        out
    }

    pub fn check_integrations(&self) -> Vec<u64> {
        self.rules
            .get("required_status_checks")
            .and_then(|p| p.as_ref())
            .and_then(|p| p["required_status_checks"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| c["integration_id"].as_u64())
            .collect()
    }
}

/// Convert a declaration to the normalized representation. `current` is the current state of the
/// same name.
///
/// Rules are handled as follows.
/// - Rules absent from the declaration are disabled (same as `false`)
/// - Rules [`Rules`] does not know cannot be declared, so they keep their current values
/// - Bypass actors of kinds ghrs does not know are kept for the same reason
/// - Parameters not written in a rule's table inherit the current value. PUT is a full
///   replacement, so this avoids resetting parameters the declaration does not touch to defaults
pub fn desired(
    decl: &Ruleset,
    current: Option<&RulesetState>,
    dir: &Directory,
) -> Result<RulesetState> {
    let at = format!("ruleset \"{}\"", decl.name);

    let mut actors: Vec<Actor> = decl
        .bypass_actors
        .iter()
        .map(|a| {
            let actor_type = actor_type_to_api(&a.actor_type)
                .with_context(|| format!("{at}: unknown bypass actor type \"{}\"", a.actor_type))?;
            let name = a.name.as_deref().unwrap_or_default();
            let without_id = ACTORS_WITHOUT_ID.contains(&a.actor_type.as_str());
            let actor_id = match a.actor_type.as_str() {
                "integration" => dir.app_id(name),
                "repository_role" => role_id(name),
                "team" => dir.team_id(name),
                _ => None,
            };
            if actor_id.is_none() && !without_id {
                bail!("{at}: {} \"{name}\" is not resolved", a.actor_type);
            }
            Ok(Actor {
                actor_type: actor_type.to_string(),
                actor_id,
                mode: a.mode.clone(),
            })
        })
        .collect::<Result<_>>()?;
    // Kinds ghrs does not know cannot be declared, so they keep their current entries
    actors.extend(
        current
            .into_iter()
            .flat_map(|c| &c.actors)
            .filter(|a| actor_type_to_toml(&a.actor_type).is_none())
            .cloned(),
    );

    let mut rules = BTreeMap::new();
    let mut missing_params = Vec::new();
    for (key, value) in &decl.rules.entries() {
        let rule = rule_to_api(key);
        // Newly created rules have no current value to inherit, so fill required parameters with
        // their defaults
        let current_params = current
            .and_then(|c| c.rules.get(rule))
            .cloned()
            .flatten()
            .or_else(|| rule_defaults(rule));
        let params = match value {
            Json::Bool(false) => continue,
            // A parameterized rule written as a boolean is treated the same as a table that
            // declares no parameters
            Json::Bool(true) => {
                current_params.or_else(|| PARAMETERIZED_RULES.contains(&rule).then(|| json!({})))
            }
            Json::Object(table) => {
                let mut merged = match current_params {
                    Some(Json::Object(map)) => map,
                    _ => Map::new(),
                };
                merge_params(&mut merged, declared_params(rule, table, dir, &at)?);
                Some(Json::Object(merged))
            }
            _ => bail!("{at}: rules.{key} must be a boolean or a table"),
        };
        if let Some((_, required)) = REQUIRED_PARAMS.iter().find(|(r, _)| *r == rule) {
            let missing: Vec<&str> = required
                .iter()
                .filter(|key| params.as_ref().and_then(|p| p.get(**key)).is_none())
                .copied()
                .collect();
            if !missing.is_empty() {
                missing_params.push(missing_params_error(decl, key, &missing));
            }
        }
        // Required by the REST API description inside the table, so REQUIRED_PARAMS cannot hold
        // it. A restriction GitHub already has keeps its value through the merge.
        if let Some(restriction) = params.as_ref().and_then(|p| p.get("dismissal_restriction"))
            && restriction.get("enabled").is_none()
        {
            missing_params.push(missing_params_error(
                decl,
                &format!("{key}.dismissal_restriction"),
                &["enabled"],
            ));
        }
        rules.insert(rule.to_string(), params);
    }
    if !missing_params.is_empty() {
        return Err(Diagnostics::new(missing_params).into());
    }
    if let Some(current) = current {
        for (rule, params) in &current.rules {
            if !Rules::KEYS.contains(&rule.as_str()) {
                rules.insert(rule.clone(), params.clone());
            }
        }
    }

    Ok(RulesetState {
        id: current.and_then(|c| c.id),
        name: decl.name.clone(),
        target: decl.target.clone(),
        enforcement: decl.enforcement.clone(),
        include: decl.conditions.include.clone(),
        exclude: decl.conditions.exclude.clone(),
        extra_conditions: current
            .map(|c| c.extra_conditions.clone())
            .unwrap_or_default(),
        actors,
        rules: normalize(rules),
    })
}

/// Normalize rules that have the same meaning but different representations into one shape.
///
/// For `update`, `update_allows_fetch_and_merge = false` is the default, and in that case GitHub
/// returns `{"type": "update"}` without parameters (observed). The normal form has no parameters,
/// also for the empty table a new `update = true` starts from.
///
/// `copilot_code_review` is the opposite: even when sent without parameters or with only some,
/// GitHub returns it with missing keys filled with false (observed). Only old rulesets are
/// returned without parameters. The normal form has all keys, so changes are shown per field.
///
/// `code_coverage` takes percentages as numbers, so `80` and `80.0` mean the same. The model
/// reads them as floats, so the normal form writes whole numbers as integers.
fn normalize(mut rules: BTreeMap<String, Option<Json>>) -> BTreeMap<String, Option<Json>> {
    if let Some(Some(Json::Object(params))) = rules.get_mut("code_coverage") {
        for value in params.values_mut() {
            if let Some(f) = value.as_f64()
                && f.fract() == 0.0
                && f.abs() < 1e15
            {
                *value = json!(f as i64);
            }
        }
    }
    if let Some(params) = rules.get_mut("update")
        && (*params == Some(json!({ "update_allows_fetch_and_merge": false }))
            || *params == Some(json!({})))
    {
        *params = None;
    }
    if let Some(params) = rules.get_mut("copilot_code_review") {
        let mut map = match params.take() {
            Some(Json::Object(map)) => map,
            Some(other) => {
                *params = Some(other);
                return rules;
            }
            None => Map::new(),
        };
        for key in ["review_draft_pull_requests", "review_on_push"] {
            map.entry(key).or_insert(Json::Bool(false));
        }
        *params = Some(Json::Object(map));
    }
    rules
}

/// Map a rule's table to API parameters. Only `required_status_checks` is converted, because
/// the TOML side uses different names.
/// Only the user can decide these values, so there is no fix to suggest. Whether one is missing
/// depends on the current state, so this is found at plan time rather than by `validate::check`.
fn missing_params_error(decl: &Ruleset, table: &str, missing: &[&str]) -> Diagnostic {
    Diagnostic::error(
        Code::MissingRequiredParams,
        format!(
            "ruleset \"{}\": rules.{table} is missing required parameters: {}",
            decl.name,
            missing.join(", ")
        ),
    )
    .at(format!(
        "{}.rules.{table}",
        validate::entry_path("ruleset", &decl.name)
    ))
    .help("a new rule needs every required parameter; GitHub returns a 422 without a reason")
}

/// Merge declared parameters into the current ones. Nested tables are merged key by key, so a
/// key not written in them keeps its current value as well. Arrays are replaced as a whole.
fn merge_params(current: &mut Map<String, Json>, declared: Map<String, Json>) {
    for (key, value) in declared {
        match (current.get_mut(&key), value) {
            (Some(Json::Object(have)), Json::Object(want)) => merge_params(have, want),
            (_, value) => {
                current.insert(key, value);
            }
        }
    }
}

fn declared_params(
    rule: &str,
    table: &Map<String, Json>,
    dir: &Directory,
    at: &str,
) -> Result<Map<String, Json>> {
    let mut out = Map::new();
    for (key, value) in table {
        let (key, value) = match (rule, key.as_str()) {
            ("required_status_checks", "require_up_to_date") => (
                "strict_required_status_checks_policy".to_string(),
                value.clone(),
            ),
            ("required_status_checks", "checks") => (
                "required_status_checks".to_string(),
                status_checks(value, dir, at)?,
            ),
            _ => (key.clone(), value.clone()),
        };
        out.insert(key, value);
    }
    if rule == "pull_request" {
        let mut params = Json::Object(out);
        for_each_review_actor(&mut params, |actor| review_actor(actor, dir, at))?;
        let Json::Object(out) = params else {
            unreachable!()
        };
        return Ok(out);
    }
    Ok(out)
}

/// Map a declaration's `{ type = "team", name = "..." }` to the API's
/// `{"id": ..., "type": "Team"}`.
fn review_actor(actor: &mut Json, dir: &Directory, at: &str) -> Result<()> {
    let kind = actor["type"].as_str().unwrap_or_default();
    let name = actor["name"].as_str().unwrap_or_default();
    let api = review_actor_type_to_api(kind)
        .with_context(|| format!("{at}: unknown actor type \"{kind}\" in pull_request"))?;
    let id = match kind {
        "team" => dir.team_id(name),
        "user" => dir.user_id(name),
        "integration" => dir.installation_id(name),
        "repository_role" => role_id(name),
        _ => None,
    }
    .with_context(|| format!("{at}: {kind} \"{name}\" in pull_request is not resolved"))?;
    *actor = json!({ "id": id, "type": api });
    Ok(())
}

fn status_checks(value: &Json, dir: &Directory, at: &str) -> Result<Json> {
    let mut checks = Vec::new();
    for check in value.as_array().into_iter().flatten() {
        let context = check
            .get("context")
            .and_then(|v| v.as_str())
            .with_context(|| format!("{at}: status check has no context"))?;
        let mut entry = Map::new();
        entry.insert("context".to_string(), json!(context));
        if let Some(name) = check.get("integration").and_then(|v| v.as_str()) {
            let id = dir
                .app_id(name)
                .with_context(|| format!("{at}: integration \"{name}\" is not resolved"))?;
            entry.insert("integration_id".to_string(), json!(id));
        }
        checks.push(Json::Object(entry));
    }
    Ok(Json::Array(checks))
}

/// Tokens without admin permission do not get `bypass_actors`. The field is missing entirely
/// rather than being an empty array (observed: read permission, push permission). Reading this
/// as zero entries would wrongly conclude that every declared bypass actor must be added, so it
/// is an error.
pub fn require_bypass_actors(v: &Json, name: &str) -> Result<()> {
    if v.get("bypass_actors").is_none() {
        bail!(
            "cannot read bypass_actors of ruleset \"{name}\"; \
            the token needs admin permission on the repository"
        );
    }
    Ok(())
}

fn str_field(v: &Json, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("response has no {key}"))
}

fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::SelfApp;

    fn decl(body: &str) -> Ruleset {
        let toml = format!("target = \"branch\"\nenforcement = \"active\"\n{body}");
        let mut decl: Ruleset = toml::from_str(&toml).unwrap();
        decl.name = "r".to_string();
        decl
    }

    fn current(rules: Json) -> RulesetState {
        RulesetState::from_api(&json!({
            "id": 42,
            "name": "r",
            "target": "branch",
            "enforcement": "active",
            "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
            "bypass_actors": [],
            "rules": rules,
        }))
        .unwrap()
    }

    #[test]
    fn undeclared_parameters_keep_current_values() {
        let cur = current(json!([{
            "type": "pull_request",
            "parameters": {
                "required_approving_review_count": 0,
                "dismiss_stale_reviews_on_push": true,
            }
        }]));
        let want = desired(
            &decl("[rules.pull_request]\nrequired_approving_review_count = 1"),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(
            want.rules["pull_request"],
            Some(json!({
                "required_approving_review_count": 1,
                "dismiss_stale_reviews_on_push": true,
            }))
        );
        assert_eq!(want.id, Some(42));
    }

    /// An empty array is a declaration to "empty it", not to "inherit"
    #[test]
    fn declared_empty_arrays_replace_current_values() {
        let cur = current(json!([{
            "type": "pull_request",
            "parameters": { "allowed_merge_methods": ["squash"] }
        }]));
        let want = desired(
            &decl("[rules.pull_request]\nallowed_merge_methods = []"),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(
            want.rules["pull_request"],
            Some(json!({ "allowed_merge_methods": [] }))
        );
    }

    #[test]
    fn undeclared_rules_are_disabled() {
        let cur = current(json!([{ "type": "deletion" }, { "type": "non_fast_forward" }]));
        let want = desired(
            &decl("[rules]\nrestrict_deletions = true\nblock_force_pushes = false"),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(want.rules.keys().collect::<Vec<_>>(), ["deletion"]);
        assert_eq!(want.rules["deletion"], None);
    }

    /// A rule GitHub added after ghrs cannot be declared, so it must not be deleted either
    #[test]
    fn unknown_rules_keep_current_values() {
        let cur = current(json!([
            { "type": "deletion" },
            { "type": "future_rule", "parameters": { "x": 1 } },
        ]));
        let want = desired(&decl(""), Some(&cur), &Directory::default()).unwrap();
        assert_eq!(want.rules.keys().collect::<Vec<_>>(), ["future_rule"]);
        assert_eq!(want.rules["future_rule"], Some(json!({ "x": 1 })));
    }

    /// A bypass actor kind ghrs does not know cannot be declared, so it must not be deleted
    #[test]
    fn unknown_bypass_actors_keep_current_values() {
        let cur = RulesetState::from_api(&json!({
            "id": 42, "name": "r", "target": "branch", "enforcement": "active",
            "conditions": { "ref_name": { "include": [], "exclude": [] } },
            "bypass_actors": [
                { "actor_id": 7, "actor_type": "User", "bypass_mode": "always" },
                { "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always" },
            ],
            "rules": [],
        }))
        .unwrap();
        let want = desired(&decl(""), Some(&cur), &Directory::default()).unwrap();
        assert_eq!(want.actors.len(), 1);
        assert_eq!(want.actors[0].actor_type, "User");
        assert_eq!(want.actors[0].actor_id, Some(7));
    }

    #[test]
    fn code_coverage_compares_whole_numbers_as_integers() {
        let cur = current(json!([{
            "type": "code_coverage",
            "parameters": { "minimum_coverage": 80, "max_coverage_drop": 0.5 }
        }]));
        let want = desired(
            &decl("[rules.code_coverage]\nminimum_coverage = 80\nmax_coverage_drop = 0.5"),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(want.rules["code_coverage"], cur.rules["code_coverage"]);
        assert_eq!(
            want.rules["code_coverage"],
            Some(json!({ "minimum_coverage": 80, "max_coverage_drop": 0.5 }))
        );
    }

    #[test]
    fn new_rules_get_required_defaults() {
        let ruleset = decl("[rules.pull_request]\nrequired_approving_review_count = 1");
        let want = desired(&ruleset, None, &Directory::default()).unwrap();
        assert_eq!(
            want.rules["pull_request"],
            Some(json!({
                "dismiss_stale_reviews_on_push": false,
                "require_code_owner_review": false,
                "require_last_push_approval": false,
                "required_approving_review_count": 1,
                "required_review_thread_resolution": false,
            }))
        );

        let cur = current(json!([{
            "type": "pull_request",
            "parameters": {
                "require_code_owner_review": true,
                "required_approving_review_count": 0,
            }
        }]));
        let want = desired(&ruleset, Some(&cur), &Directory::default()).unwrap();
        assert_eq!(
            want.rules["pull_request"],
            Some(
                json!({ "require_code_owner_review": true, "required_approving_review_count": 1 })
            )
        );

        let want = desired(
            &decl("[rules]\nrequired_status_checks = true"),
            None,
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(
            want.rules["required_status_checks"],
            Some(json!({
                "required_status_checks": [],
                "strict_required_status_checks_policy": false,
            }))
        );
    }

    #[test]
    fn rejects_new_rules_without_required_params() {
        let err = desired(
            &decl("[rules]\ncommit_message_pattern = true"),
            None,
            &Directory::default(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("operator, pattern"), "{err}");

        let partial = decl("[rules.commit_message_pattern]\noperator = \"starts_with\"");
        assert!(desired(&partial, None, &Directory::default()).is_err());

        // An existing rule is filled with the current values
        let cur = current(json!([{
            "type": "commit_message_pattern",
            "parameters": { "operator": "starts_with", "pattern": "feat", "negate": false }
        }]));
        assert!(desired(&partial, Some(&cur), &Directory::default()).is_ok());
    }

    /// Keys not written in a nested table keep their current values
    #[test]
    fn nested_tables_merge_with_current_parameters() {
        let cur = current(json!([{ "type": "pull_request", "parameters": {
            "required_approving_review_count": 1,
            "dismissal_restriction": {
                "enabled": true,
                "allowed_actors": [{ "id": 5, "type": "RepositoryRole" }],
            },
        } }]));
        let want = desired(
            &decl("[rules.pull_request.dismissal_restriction]\nenabled = true"),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(want.rules["pull_request"], cur.rules["pull_request"]);

        let want = desired(
            &decl(
                "[rules.pull_request.dismissal_restriction]\n\
                    allowed_actors = [{ type = \"repository_role\", name = \"admin\" }]",
            ),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(want.rules["pull_request"], cur.rules["pull_request"]);
    }

    #[test]
    fn new_dismissal_restriction_needs_enabled() {
        let decl = decl(
            "[rules.pull_request.dismissal_restriction]\n\
                allowed_actors = [{ type = \"repository_role\", name = \"admin\" }]",
        );
        let Err(err) = desired(&decl, None, &Directory::default()) else {
            panic!("accepted a dismissal_restriction without enabled");
        };
        let d = err.downcast::<Diagnostics>().unwrap();
        assert_eq!(d.diagnostics[0].code, Code::MissingRequiredParams);
        assert_eq!(
            d.diagnostics[0].path.as_deref(),
            Some("ruleset.r.rules.pull_request.dismissal_restriction")
        );
    }

    #[test]
    fn update_true_keeps_current_parameters() {
        let cur = current(json!([{
            "type": "update", "parameters": { "update_allows_fetch_and_merge": true },
        }]));
        let want = desired(
            &decl("[rules]\nrestrict_updates = true"),
            Some(&cur),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(want.rules["update"], cur.rules["update"]);
    }

    #[test]
    fn update_without_fetch_and_merge_has_no_parameters() {
        let sugar = desired(
            &decl("[rules]\nrestrict_updates = true"),
            None,
            &Directory::default(),
        )
        .unwrap();
        let table = desired(
            &decl("[rules.restrict_updates]\nupdate_allows_fetch_and_merge = false"),
            None,
            &Directory::default(),
        )
        .unwrap();
        let api = current(json!([{ "type": "update" }]));
        assert_eq!(sugar.rules["update"], None);
        assert_eq!(table.rules["update"], None);
        assert_eq!(api.rules["update"], None);

        let fetch = desired(
            &decl("[rules.restrict_updates]\nupdate_allows_fetch_and_merge = true"),
            None,
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(
            fetch.rules["update"],
            Some(json!({ "update_allows_fetch_and_merge": true }))
        );
    }

    /// GitHub returns missing keys filled with false
    #[test]
    fn copilot_code_review_fills_missing_parameters() {
        let full = json!({ "review_draft_pull_requests": false, "review_on_push": false });
        let legacy = current(json!([{ "type": "copilot_code_review", "parameters": null }]));
        assert_eq!(legacy.rules["copilot_code_review"], Some(full.clone()));

        let new = desired(
            &decl("[rules]\ncopilot_code_review = true"),
            None,
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(new.rules["copilot_code_review"], Some(full));

        let partial = desired(
            &decl("[rules.copilot_code_review]\nreview_on_push = true"),
            Some(&legacy),
            &Directory::default(),
        )
        .unwrap();
        assert_eq!(
            partial.rules["copilot_code_review"],
            Some(json!({ "review_draft_pull_requests": false, "review_on_push": true }))
        );
    }

    #[test]
    fn review_actors_use_api_ids() {
        let dir = Directory::default()
            .with_team("reviewers", 7)
            .with_user("Octocat", 9);
        let want = desired(
            &decl(
                "[rules.pull_request]\n\
                required_reviewers = [{ file_patterns = [\"*.rs\"], minimum_approvals = 1, \
                    reviewer = { type = \"team\", name = \"reviewers\" } }]\n\
                dismissal_restriction = { enabled = true, allowed_actors = [\
                    { type = \"repository_role\", name = \"admin\" }, { type = \"user\", name = \
                        \"octocat\" }] }",
            ),
            None,
            &dir,
        )
        .unwrap();
        let params = want.rules["pull_request"].as_ref().unwrap();
        assert_eq!(
            params["required_reviewers"][0]["reviewer"],
            json!({ "id": 7, "type": "Team" })
        );
        assert_eq!(
            params["dismissal_restriction"]["allowed_actors"],
            json!([{ "id": 5, "type": "RepositoryRole" }, { "id": 9, "type": "User" }])
        );
        assert_eq!(
            want.review_actors(),
            [
                ("Team".to_string(), 7),
                ("RepositoryRole".to_string(), 5),
                ("User".to_string(), 9)
            ]
        );

        let unresolved = desired(
            &decl(
                "[rules.pull_request]\nrequired_reviewers = [{ file_patterns = [\"*\"], \
                minimum_approvals = 1, reviewer = { type = \"team\", name = \"x\" } }]",
            ),
            None,
            &Directory::default(),
        );
        assert!(unresolved.is_err());
    }

    /// Apps in pull_request are referred to by installation ID, unlike bypass actors
    #[test]
    fn apps_in_pull_request_use_installation_ids() {
        let ruleset = |name: &str| {
            decl(&format!(
                "[rules.pull_request.dismissal_restriction]\nenabled = true\n\
                allowed_actors = [{{ type = \"integration\", name = \"{name}\" }}]"
            ))
        };
        let actors = |want: &RulesetState| {
            want.rules["pull_request"].as_ref().unwrap()["dismissal_restriction"]["allowed_actors"]
                .clone()
        };

        let dir = Directory::default()
            .with_app("approver", 4533362)
            .with_installation("approver", 152340499);
        let want = desired(&ruleset("approver"), None, &dir).unwrap();
        assert_eq!(
            actors(&want),
            json!([{ "id": 152340499, "type": "IntegrationInstallation" }])
        );
        assert!(desired(&ruleset("approver"), None, &Directory::default()).is_err());

        let mut dir = Directory::default();
        dir.add_self(&SelfApp {
            id: 4533362,
            installation_id: 152340499,
        });
        let want = desired(&ruleset("self"), None, &dir).unwrap();
        assert_eq!(
            actors(&want),
            json!([{ "id": 152340499, "type": "IntegrationInstallation" }])
        );
        assert_eq!(dir.app_id("self"), Some(4533362));
    }

    #[test]
    fn status_checks_use_api_names_and_ids() {
        let dir = Directory::default().with_app("github-actions", 15368);
        let want = desired(
            &decl(
                "[rules.required_status_checks]\nrequire_up_to_date = true\n\
                checks = [{ context = \"test\" }, { context = \"build\", integration = \
                    \"github-actions\" }]",
            ),
            None,
            &dir,
        )
        .unwrap();
        assert_eq!(
            want.rules["required_status_checks"],
            Some(json!({
                "strict_required_status_checks_policy": true,
                "required_status_checks": [
                    { "context": "test" },
                    { "context": "build", "integration_id": 15368 },
                ],
            }))
        );
    }

    #[test]
    fn unresolved_names_are_errors() {
        let ruleset = decl("[[bypass_actors]]\ntype = \"integration\"\nname = \"renovate\"");
        assert!(desired(&ruleset, None, &Directory::default()).is_err());

        let dir = Directory::default().with_app("renovate", 2740);
        let want = desired(&ruleset, None, &dir).unwrap();
        assert_eq!(want.actors[0].actor_id, Some(2740));
        assert_eq!(want.actors[0].mode, "always");
    }

    #[test]
    fn api_body_is_complete_and_writable() {
        let api = json!({
            "id": 42,
            "name": "r",
            "target": "branch",
            "source_type": "Repository",
            "source": "takumin/ghrs",
            "enforcement": "active",
            "node_id": "RRS_x",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "current_user_can_bypass": "always",
            "_links": {},
            "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
            "bypass_actors": [
                { "actor_id": null, "actor_type": "OrganizationAdmin", "bypass_mode": "always" },
                { "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "pull_request" },
            ],
            "rules": [
                { "type": "deletion" },
                { "type": "pull_request", "parameters": { "required_approving_review_count": 1 } },
            ],
        });
        let body = RulesetState::from_api(&api).unwrap().to_api();
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "bypass_actors",
                "conditions",
                "enforcement",
                "name",
                "rules",
                "target"
            ]
        );
        for key in ["bypass_actors", "conditions", "rules"] {
            assert_eq!(body[key], api[key], "{key}");
        }
    }

    /// Tokens without admin permission do not get bypass_actors
    #[test]
    fn missing_bypass_actors_is_an_error() {
        let v = json!({
            "name": "r", "target": "branch", "enforcement": "active",
            "current_user_can_bypass": "never", "rules": [],
        });
        assert!(RulesetState::from_api(&v).is_err());

        let mut v = v;
        v["bypass_actors"] = json!([]);
        assert!(RulesetState::from_api(&v).unwrap().actors.is_empty());
    }

    #[test]
    fn keeps_unknown_conditions() {
        let mut cur = current(json!([]));
        cur.extra_conditions
            .insert("repository_name".to_string(), json!({ "include": ["x"] }));
        let want = desired(&decl(""), Some(&cur), &Directory::default()).unwrap();
        assert!(want.extra_conditions.contains_key("repository_name"));
    }
}
