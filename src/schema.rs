//! Mapping between GitHub's vocabulary and `.ghrs.toml`'s vocabulary. Conversion tables live
//! only here.

use serde_json::{Value as Json, json};

use crate::model::Rules;

/// The GitHub REST API description the rule types in [`Rules`] were checked against.
///
/// `task test:rest-api` downloads this file and checks that the rule types and their parameters
/// still match it. Change the commit only together with that check.
pub const REST_API_DESCRIPTION: &str = concat!(
    "https://raw.githubusercontent.com/github/rest-api-description/",
    "755ee33b4a81d51f6f07a902cce26cbec3b822f2",
    "/descriptions/api.github.com/api.github.com.json",
);

/// Name of the schema that defines a rule type in [`REST_API_DESCRIPTION`]. Every rule follows
/// this naming (`pull_request` -> `repository-rule-pull-request`).
fn rule_schema(rule: &str) -> String {
    format!("repository-rule-{}", rule.replace('_', "-"))
}

/// URL of a schema in [`REST_API_DESCRIPTION`] (`repository-ruleset` ->
/// `...api.github.com.json#/components/schemas/repository-ruleset`).
pub fn rest_api_schema(name: &str) -> String {
    format!("{REST_API_DESCRIPTION}#/components/schemas/{name}")
}

/// URL of the schema that defines a rule type in [`REST_API_DESCRIPTION`].
pub fn rest_api_rule(rule: &str) -> String {
    rest_api_schema(&rule_schema(rule))
}

/// Where [`REST_API_DESCRIPTION`] defines what a declaration path under `rules` may contain
/// (`ruleset.main.rules.pull_request.required_reviewers[0]` -> the `pull_request` rule). A rule
/// ghrs does not know refers to the list of all rules. `None` outside `rules`.
pub fn rule_reference(path: &str) -> Option<String> {
    let rest = path.strip_prefix("ruleset.")?;
    // Skip the ruleset name, which is a bare key or a JSON-quoted string
    let rest = match rest.strip_prefix('"') {
        Some(quoted) => {
            let mut escaped = false;
            let end = quoted.char_indices().find(|&(_, c)| {
                let closes = c == '"' && !escaped;
                escaped = c == '\\' && !escaped;
                closes
            })?;
            &quoted[end.0 + 1..]
        }
        None => &rest[rest.find('.')?..],
    };
    let rest = rest.strip_prefix(".rules")?;
    let rule = match rest.strip_prefix('.') {
        Some(rest) => &rest[..rest.find(['.', '[']).unwrap_or(rest.len())],
        None if rest.is_empty() => "",
        None => return None,
    };
    let api = rule_to_api(rule);
    let known = Rules::KEYS.contains(&api) && rule_to_toml(api) == rule;
    if known || PUSH_RULES.contains(&rule) {
        Some(rest_api_rule(api))
    } else {
        Some(rest_api_schema("repository-rule"))
    }
}

/// Rules whose API type names only what they restrict, so `deletion = true` could be read as
/// allowing deletion. The TOML side uses the names the web UI shows. `(API, TOML)`; every other
/// rule has the same name on both sides.
pub const RULE_NAMES: &[(&str, &str)] = &[
    ("creation", "restrict_creations"),
    ("update", "restrict_updates"),
    ("deletion", "restrict_deletions"),
    ("non_fast_forward", "block_force_pushes"),
];

pub fn rule_to_toml(api: &str) -> &str {
    RULE_NAMES
        .iter()
        .find(|(a, _)| *a == api)
        .map_or(api, |(_, t)| *t)
}

pub fn rule_to_api(toml: &str) -> &str {
    RULE_NAMES
        .iter()
        .find(|(_, t)| *t == toml)
        .map_or(toml, |(a, _)| *a)
}

/// IDs of built-in repository roles that can be bypass actors.
///
/// GitHub does not document these officially. Observed via GraphQL `repositoryRoleName`.
/// Warn when an ID not in the table is observed.
pub const REPOSITORY_ROLES: &[(u64, &str)] = &[(2, "maintain"), (4, "write"), (5, "admin")];

/// Built-in roles without write permission. Making one a bypass actor yields 422 (`Actor base role
/// does not have write permissions`) (observed: IDs 1 and 3; which is which is not distinguished).
pub const ROLES_WITHOUT_WRITE: &[&str] = &["read", "triage"];

/// Rules known to have parameters. A rule listed here and written as `true` is sent with an
/// empty parameter object rather than without parameters.
///
/// Derived from the members of the GraphQL `RuleParameters` union, which matches the rules with
/// `parameters` in the `repository-rule` schema of the REST API description.
pub const PARAMETERIZED_RULES: &[&str] = &[
    "update",
    "pull_request",
    "required_status_checks",
    "required_deployments",
    "commit_message_pattern",
    "commit_author_email_pattern",
    "committer_email_pattern",
    "branch_name_pattern",
    "tag_name_pattern",
    "merge_queue",
    "code_scanning",
    "code_quality",
    "code_coverage",
    "copilot_code_review",
    "workflows",
    "file_extension_restriction",
    "file_path_restriction",
    "max_file_path_length",
    "max_file_size",
];

/// Required parameters with no default value. Only the user can decide these values, so a
/// missing one is an error at plan time. GitHub returns a 422 without a reason (observed).
pub const REQUIRED_PARAMS: &[(&str, &[&str])] = &[
    (
        "merge_queue",
        &[
            "check_response_timeout_minutes",
            "grouping_strategy",
            "max_entries_to_build",
            "max_entries_to_merge",
            "merge_method",
            "min_entries_to_merge",
            "min_entries_to_merge_wait_minutes",
        ],
    ),
    ("commit_message_pattern", &["operator", "pattern"]),
    ("commit_author_email_pattern", &["operator", "pattern"]),
    ("committer_email_pattern", &["operator", "pattern"]),
    ("branch_name_pattern", &["operator", "pattern"]),
    ("tag_name_pattern", &["operator", "pattern"]),
    (
        "required_deployments",
        &["required_deployment_environments"],
    ),
    ("code_scanning", &["code_scanning_tools"]),
    // Required in the REST API description; not observed
    ("code_quality", &["severity"]),
    ("workflows", &["workflows"]),
];

/// Rules usable only in organization-owned repositories. In personal repositories GitHub returns
/// a 422 without a reason (`Invalid rule '<type>': `) (observed).
pub const ORG_ONLY_RULES: &[&str] = &[
    "merge_queue",
    "commit_message_pattern",
    "commit_author_email_pattern",
    "committer_email_pattern",
    "branch_name_pattern",
    "tag_name_pattern",
];

/// Rules usable only in branch rulesets. In tag rulesets GitHub returns a 422 without a reason
/// (`Invalid rule '<type>': `) (observed).
pub const BRANCH_ONLY_RULES: &[&str] = &[
    "pull_request",
    "copilot_code_review",
    "code_scanning",
    "merge_queue",
    "branch_name_pattern",
];

/// Rules usable only in tag rulesets. In branch rulesets they yield a 422 without a reason
/// (observed).
pub const TAG_ONLY_RULES: &[&str] = &["tag_name_pattern"];

/// Rules for push rulesets. Push rulesets are Team / Enterprise only, and in
/// branch / tag rulesets they yield a 422 without a reason (observed: personal repositories and
/// free organizations).
pub const PUSH_RULES: &[&str] = &[
    "file_path_restriction",
    "file_extension_restriction",
    "max_file_path_length",
    "max_file_size",
];

/// Shape of include / exclude patterns in `conditions`. Except for special patterns, a
/// per-target prefix is required; without it the result is 422
/// (`Invalid target patterns: '<pattern>'`) (observed).
///
/// `~DEFAULT_BRANCH` is accepted by tag rulesets too (observed), but its meaning for tags is not
/// obvious, so it is allowed only for branches.
pub fn ref_patterns(target: &str) -> Option<(&'static str, &'static [&'static str])> {
    match target {
        "branch" => Some(("refs/heads/", &["~ALL", "~DEFAULT_BRANCH"])),
        "tag" => Some(("refs/tags/", &["~ALL"])),
        _ => None,
    }
}

/// Defaults for rules with required parameters. Fills parameters absent from the declaration in
/// newly created rules.
///
/// On creation GitHub returns 422 (`Invalid property /rules/N: data matches no possible input`)
/// unless these are all present (observed: `pull_request`). Values match the initial values when
/// a rule is added in the UI (everything off, 0 approvals).
pub fn rule_defaults(rule: &str) -> Option<Json> {
    Some(match rule {
        "pull_request" => json!({
            "dismiss_stale_reviews_on_push": false,
            "require_code_owner_review": false,
            "require_last_push_approval": false,
            "required_approving_review_count": 0,
            "required_review_thread_resolution": false,
        }),
        "required_status_checks" => json!({
            "required_status_checks": [],
            "strict_required_status_checks_policy": false,
        }),
        _ => return None,
    })
}

/// Bypass actor kind. Mapping between the API's `actor_type` and the TOML `type`.
pub const ACTOR_TYPES: &[(&str, &str)] = &[
    ("Integration", "integration"),
    ("OrganizationAdmin", "organization_admin"),
    ("RepositoryRole", "repository_role"),
    ("Team", "team"),
    ("DeployKey", "deploy_key"),
];

/// Kind of actor referenced as `{id, type}` in `pull_request` parameters. Mapping between the
/// API's `type` and the TOML `type`. `required_reviewers[].reviewer` accepts only Team (an App
/// yields 422 `data matches no possible input`), while `dismissal_restriction.allowed_actors[]`
/// accepts all four kinds (observed).
///
/// The `id` of an App is its installation ID, unlike bypass actors, which use the App ID. An App
/// ID yields a 422 that does not name the cause (`Dismissal apps must have write access to the
/// repository` or `Invalid node ID format`) (observed).
pub const REVIEW_ACTOR_TYPES: &[(&str, &str)] = &[
    ("Team", "team"),
    ("RepositoryRole", "repository_role"),
    ("User", "user"),
    ("IntegrationInstallation", "integration"),
];

pub fn review_actor_type_to_toml(api: &str) -> Option<&'static str> {
    REVIEW_ACTOR_TYPES
        .iter()
        .find(|(a, _)| *a == api)
        .map(|(_, t)| *t)
}

pub fn review_actor_type_to_api(toml: &str) -> Option<&'static str> {
    REVIEW_ACTOR_TYPES
        .iter()
        .find(|(_, t)| *t == toml)
        .map(|(a, _)| *a)
}

/// Apply `f` to each value in the `pull_request` parameters that references an actor.
///
/// Targets are `required_reviewers[].reviewer` and `dismissal_restriction.allowed_actors[]`.
/// Used for both the API's `{id, type}` shape and the TOML `{type, name}` shape.
pub fn for_each_review_actor<E>(
    params: &mut Json,
    mut f: impl FnMut(&mut Json) -> Result<(), E>,
) -> Result<(), E> {
    let reviewers = params
        .get_mut("required_reviewers")
        .and_then(Json::as_array_mut);
    for reviewer in reviewers.into_iter().flatten() {
        if let Some(actor) = reviewer.get_mut("reviewer") {
            f(actor)?;
        }
    }
    let allowed = params
        .get_mut("dismissal_restriction")
        .and_then(|d| d.get_mut("allowed_actors"))
        .and_then(Json::as_array_mut);
    for actor in allowed.into_iter().flatten() {
        f(actor)?;
    }
    Ok(())
}

/// Bypass actor kinds without an ID. GitHub stores `actor_id` as null, meaning the whole kind
/// (`deploy_key` means every deploy key, not a specific key). Sending an ID still results in
/// null (observed), so declarations omit `name`.
pub const ACTORS_WITHOUT_ID: &[&str] = &["organization_admin", "deploy_key"];

pub const BYPASS_MODES: &[&str] = &["always", "pull_request", "exempt"];

/// Enforcement values available on the free plan. `evaluate` is Enterprise only.
pub const ENFORCEMENTS: &[&str] = &["active", "disabled"];

/// Repository ruleset targets. `push` is Team / Enterprise only.
pub const TARGETS: &[&str] = &["branch", "tag"];

pub const VISIBILITIES: &[&str] = &["public", "private"];

/// Values of `actions.allowed_actions`, from the narrowest to the widest.
pub const ALLOWED_ACTIONS: &[&str] = &["local_only", "selected", "all"];

/// Values of `actions.default_workflow_permissions`, from the narrowest to the widest.
pub const WORKFLOW_PERMISSIONS: &[&str] = &["read", "write"];

/// Values of `actions.fork_pr_contributor_approval`, from the narrowest to the widest set of
/// fork pull requests whose workflows run without approval.
pub const FORK_PR_APPROVAL_POLICIES: &[&str] = &[
    "all_external_contributors",
    "first_time_contributors",
    "first_time_contributors_new_to_github",
];

/// Maximum number of rulesets per repository. Creating the 76th yields 422 (`The ruleset limit
/// has been reached.`) (observed, personal repository on the free plan).
pub const MAX_RULESETS: usize = 75;

/// Collaborator and team permissions. Built-in roles only (custom roles are Enterprise only).
pub const PERMISSIONS: &[&str] = &["pull", "triage", "push", "maintain", "admin"];

// Values of rule parameters, as enumerated in REST_API_DESCRIPTION. ghrs passes them through
// without checking; they only constrain the JSON Schema of the declaration file.

pub const MERGE_METHODS: &[&str] = &["merge", "squash", "rebase"];
pub const MERGE_QUEUE_METHODS: &[&str] = &["MERGE", "SQUASH", "REBASE"];
pub const MERGE_QUEUE_GROUPING: &[&str] = &["ALLGREEN", "HEADGREEN"];
pub const PATTERN_OPERATORS: &[&str] = &["starts_with", "ends_with", "contains", "regex"];
pub const ALERTS_THRESHOLDS: &[&str] = &["none", "errors", "errors_and_warnings", "all"];
pub const SECURITY_ALERTS_THRESHOLDS: &[&str] = &[
    "none",
    "critical",
    "high_or_higher",
    "medium_or_higher",
    "all",
];
pub const CODE_QUALITY_SEVERITIES: &[&str] = &["errors", "warnings", "notes", "all"];

/// The TOML `type` of bypass actors.
pub fn actor_type_names() -> Vec<&'static str> {
    ACTOR_TYPES.iter().map(|(_, toml)| *toml).collect()
}

/// The TOML `type` of actors in `pull_request`.
pub fn review_actor_type_names() -> Vec<&'static str> {
    REVIEW_ACTOR_TYPES.iter().map(|(_, toml)| *toml).collect()
}

pub fn role_name(id: u64) -> Option<&'static str> {
    REPOSITORY_ROLES
        .iter()
        .find(|(role_id, _)| *role_id == id)
        .map(|(_, name)| *name)
}

pub fn role_id(name: &str) -> Option<u64> {
    REPOSITORY_ROLES
        .iter()
        .find(|(_, role)| *role == name)
        .map(|(id, _)| *id)
}

pub fn actor_type_to_toml(api: &str) -> Option<&'static str> {
    ACTOR_TYPES.iter().find(|(a, _)| *a == api).map(|(_, t)| *t)
}

pub fn actor_type_to_api(toml: &str) -> Option<&'static str> {
    ACTOR_TYPES
        .iter()
        .find(|(_, t)| *t == toml)
        .map(|(a, _)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn rule_references_point_into_the_rest_api_description() {
        let schema = |name: &str| format!("{REST_API_DESCRIPTION}#/components/schemas/{name}");
        assert_eq!(
            rule_reference("ruleset.main.rules.pull_request.required_reviewers[0].reviewer"),
            Some(schema("repository-rule-pull-request"))
        );
        assert_eq!(
            rule_reference(r#"ruleset."release/v1.x".rules.block_force_pushes"#),
            Some(schema("repository-rule-non-fast-forward"))
        );
        // The API's name is not a key of the declaration
        assert_eq!(
            rule_reference("ruleset.main.rules.non_fast_forward"),
            Some(schema("repository-rule"))
        );
        assert_eq!(
            rule_reference(r#"ruleset."a\".rules".rules.max_file_size"#),
            Some(schema("repository-rule-max-file-size"))
        );
        assert_eq!(
            rule_reference("ruleset.main.rules.non_fast_foward"),
            Some(schema("repository-rule"))
        );
        assert_eq!(
            rule_reference("ruleset.main.rules"),
            Some(schema("repository-rule"))
        );
        assert_eq!(rule_reference("ruleset.main.rulesx"), None);
        assert_eq!(rule_reference("ruleset.main.bypass_actors[0]"), None);
        assert_eq!(rule_reference("repository.allow_rebase"), None);
    }

    /// Parameters the API returns but [`REST_API_DESCRIPTION`] does not document (observed).
    const UNDOCUMENTED_PARAMS: &[(&str, &str)] = &[(
        "pull_request",
        "require_extra_approval_for_unattributed_changes",
    )];

    /// [`REST_API_DESCRIPTION`], downloaded with curl once per test run. The tests that use it
    /// are run separately by `task test:rest-api`.
    fn rest_api_description() -> &'static Json {
        static API: std::sync::OnceLock<Json> = std::sync::OnceLock::new();
        API.get_or_init(|| {
            let output = std::process::Command::new("curl")
                .args(["-fsSL", "--retry", "3", REST_API_DESCRIPTION])
                .output()
                .expect("cannot run curl");
            assert!(
                output.status.success(),
                "cannot download {REST_API_DESCRIPTION}"
            );
            serde_json::from_slice(&output.stdout).unwrap()
        })
    }

    /// Checks [`Rules`] against [`REST_API_DESCRIPTION`].
    #[test]
    #[ignore = "downloads the REST API description; run task test:rest-api"]
    fn rules_match_the_rest_api_description() {
        let api = rest_api_description();
        let schemas = &api["components"]["schemas"];

        let mut documented = BTreeMap::new();
        for entry in schemas["repository-rule"]["oneOf"].as_array().unwrap() {
            let name = entry["$ref"]
                .as_str()
                .unwrap()
                .trim_start_matches("#/components/schemas/");
            let rule = schemas[name]["properties"]["type"]["enum"][0]
                .as_str()
                .unwrap();
            assert_eq!(
                name,
                rule_schema(rule),
                "rule_reference relies on this naming"
            );
            let params = &schemas[name]["properties"]["parameters"];
            let keys: BTreeSet<&str> = params["properties"]
                .as_object()
                .map(|p| p.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let required: Vec<&str> = params["required"]
                .as_array()
                .map(|r| r.iter().filter_map(Json::as_str).collect())
                .unwrap_or_default();
            documented.insert(rule, (keys, required));
        }

        let known: BTreeSet<&str> = Rules::KEYS.iter().chain(PUSH_RULES).copied().collect();
        let types: BTreeSet<&str> = documented.keys().copied().collect();
        assert_eq!(known, types, "rule types");

        for rule in Rules::KEYS {
            let (keys, required) = &documented[rule];
            let ours: BTreeSet<&str> = match *rule {
                // The TOML side uses shorter names; see state::declared_params
                "required_status_checks" => [
                    "do_not_enforce_on_create",
                    "required_status_checks",
                    "strict_required_status_checks_policy",
                ]
                .into(),
                _ => Rules::param_keys(rule)
                    .unwrap_or_default()
                    .iter()
                    .copied()
                    .collect(),
            };
            let ours: BTreeSet<&str> = ours
                .into_iter()
                .filter(|key| !UNDOCUMENTED_PARAMS.contains(&(*rule, *key)))
                .collect();
            assert_eq!(&ours, keys, "parameters of {rule}");
            assert_eq!(
                PARAMETERIZED_RULES.contains(rule),
                !keys.is_empty(),
                "{rule} in PARAMETERIZED_RULES"
            );

            // A new rule must be sent with every required parameter. `update = true` is sent
            // without parameters (observed), so it is the exception.
            if *rule == "update" {
                continue;
            }
            let user = REQUIRED_PARAMS
                .iter()
                .find(|(r, _)| r == rule)
                .map(|(_, p)| *p)
                .unwrap_or_default();
            let defaults = rule_defaults(rule).unwrap_or_default();
            for key in required {
                assert!(
                    user.contains(key) || defaults.get(key).is_some(),
                    "required parameter {rule}.{key} is neither in REQUIRED_PARAMS nor defaulted"
                );
            }
        }
    }

    /// Checks the JSON Schema of the declaration file against [`REST_API_DESCRIPTION`], down to
    /// nested parameters: every key ghrs declares exists there under its API name, a parameter
    /// object has exactly the documented keys, and enumerated values are documented ones (all of
    /// them, for parameters).
    #[test]
    #[ignore = "downloads the REST API description; run task test:rest-api"]
    fn json_schema_matches_the_rest_api_description() {
        let schemas = &rest_api_description()["components"]["schemas"];
        let resolve = |v: &'static Json| -> &'static Json {
            match v["$ref"].as_str() {
                Some(r) => &schemas[r.trim_start_matches("#/components/schemas/")],
                None => v,
            }
        };
        let ours = crate::model::json_schema();
        let defs = ours["$defs"].as_object().unwrap();
        let prefix = format!("{REST_API_DESCRIPTION}#/components/schemas/");

        // (our type, the documented object, whether it is a whole parameter object)
        let mut pairs: Vec<(&str, &Json, bool)> = Vec::new();
        for (name, def) in defs {
            // The list of all rules is checked by rules_match_the_rest_api_description
            if name == "Rules" {
                continue;
            }
            if let Some(source) = def["x-github-source"].as_str() {
                let component = source.strip_prefix(&prefix).unwrap();
                let params = component.starts_with("repository-rule-params-");
                pairs.push((name, &schemas[component], params));
            }
        }
        for (rule, property) in defs["Rules"]["properties"].as_object().unwrap() {
            if let Some(r) = property["anyOf"][1]["$ref"].as_str() {
                let params = &schemas[rule_schema(rule_to_api(rule))]["properties"]["parameters"];
                pairs.push((r.trim_start_matches("#/$defs/"), params, true));
            }
        }
        assert!(pairs.len() > Rules::KEYS.len(), "{pairs:?}");

        let enumerated = |v: &Json| -> Option<BTreeSet<String>> {
            let values = v["enum"].as_array().or(v["items"]["enum"].as_array())?;
            Some(
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            )
        };
        for (name, documented, params) in pairs {
            let documented = resolve(documented)["properties"].as_object().unwrap();
            let mut declared = BTreeSet::new();
            for (key, property) in defs[name]["properties"].as_object().unwrap() {
                if property["x-github-observed"] == true {
                    continue;
                }
                let api = match property["x-github-api-name"].as_str() {
                    Some(api) => api.split('.').next().unwrap(),
                    None => key,
                };
                assert!(
                    documented.contains_key(api),
                    "{name}.{key} ({api}) is not documented"
                );
                declared.insert(api);
                // Keys written by name have values in ghrs's vocabulary
                if property["x-github-api-name"].is_string() {
                    continue;
                }
                if let Some(values) = enumerated(property) {
                    let item = resolve(&documented[api]);
                    let item = if item["items"].is_object() {
                        resolve(&item["items"])
                    } else {
                        item
                    };
                    let allowed = enumerated(item).unwrap_or_default();
                    if params {
                        assert_eq!(values, allowed, "values of {name}.{key}");
                    } else {
                        assert!(values.is_subset(&allowed), "values of {name}.{key}");
                    }
                }
            }
            if params {
                let documented: BTreeSet<&str> = documented.keys().map(String::as_str).collect();
                assert_eq!(declared, documented, "keys of {name}");
            }
        }
    }

    /// Checks `[actions]` against [`REST_API_DESCRIPTION`]: each key exists under the schema it
    /// names, with the same enumerated values, and together the keys of `[actions]` and of
    /// `[actions.oidc_subject_claim]` are exactly what their PUT endpoints take.
    #[test]
    #[ignore = "downloads the REST API description; run task test:rest-api"]
    fn actions_match_the_rest_api_description() {
        let api = rest_api_description();
        let schemas = &api["components"]["schemas"];
        let prefix = format!("{REST_API_DESCRIPTION}#/components/schemas/");
        let ours = crate::model::json_schema();
        let defs = &ours["$defs"];
        let sent = |endpoints: &[&str]| {
            let mut sent = BTreeSet::new();
            for endpoint in endpoints {
                let path = format!("/repos/{{owner}}/{{repo}}/actions{endpoint}");
                let content = &api["paths"][&path]["put"]["requestBody"]["content"];
                let mut body = &content["application/json"]["schema"];
                if let Some(r) = body["$ref"].as_str() {
                    body = &schemas[r.trim_start_matches("#/components/schemas/")];
                }
                sent.extend(body["properties"].as_object().unwrap().keys().cloned());
            }
            sent
        };
        // Keys by the API's name
        let keys = |properties: &serde_json::Map<String, Json>| -> BTreeSet<String> {
            properties
                .iter()
                .filter(|(_, p)| p["$ref"].is_null())
                .map(|(key, p)| p["x-github-api-name"].as_str().unwrap_or(key).to_string())
                .collect()
        };

        let actions = defs["Actions"]["properties"].as_object().unwrap();
        let endpoints = [
            "/permissions",
            "/permissions/workflow",
            "/permissions/fork-pr-contributor-approval",
        ];
        assert_eq!(keys(actions), sent(&endpoints), "keys of actions");
        let oidc = defs["OidcSubjectClaim"]["properties"].as_object().unwrap();
        assert_eq!(
            keys(oidc),
            sent(&["/oidc/customization/sub"]),
            "keys of actions.oidc_subject_claim"
        );

        for (key, property) in actions {
            let Some(source) = property["x-github-source"].as_str() else {
                continue;
            };
            let mut documented = &schemas[source.strip_prefix(&prefix).unwrap()];
            assert!(documented.is_object(), "{key}: {source} is not documented");
            // A key ghrs renames is a property of the schema it names
            if let Some(name) = property["x-github-api-name"].as_str() {
                documented = &documented["properties"][name];
            }
            let values = |v: &Json| -> BTreeSet<String> {
                v["enum"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            };
            assert_eq!(values(property), values(documented), "values of {key}");
        }
    }

    /// Before changing the constant tables, record the observed evidence first.
    #[test]
    fn repository_roles_are_pinned() {
        assert_eq!(
            REPOSITORY_ROLES,
            &[(2, "maintain"), (4, "write"), (5, "admin")]
        );
    }

    #[test]
    fn rule_names_round_trip() {
        for (api, toml) in RULE_NAMES {
            assert_eq!(rule_to_toml(api), *toml);
            assert_eq!(rule_to_api(toml), *api);
        }
        assert_eq!(rule_to_toml("pull_request"), "pull_request");
        assert_eq!(rule_to_api("pull_request"), "pull_request");
    }

    #[test]
    fn actor_types_round_trip() {
        for (api, toml) in ACTOR_TYPES {
            assert_eq!(actor_type_to_toml(api), Some(*toml));
            assert_eq!(actor_type_to_api(toml), Some(*api));
        }
    }
}
