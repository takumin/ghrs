//! Mapping between GitHub's vocabulary and `.ghrs.toml`'s vocabulary. Conversion tables live
//! only here.

use serde_json::{Value as Json, json};

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
/// API's `type` and the TOML `type`. `required_reviewers[].reviewer` accepts only Team, while
/// `dismissal_restriction.allowed_actors[]` accepts all three kinds (observed).
pub const REVIEW_ACTOR_TYPES: &[(&str, &str)] = &[
    ("Team", "team"),
    ("RepositoryRole", "repository_role"),
    ("User", "user"),
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

/// Maximum number of rulesets per repository. Creating the 76th yields 422 (`The ruleset limit
/// has been reached.`) (observed, personal repository on the free plan).
pub const MAX_RULESETS: usize = 75;

/// Collaborator and team permissions. Built-in roles only (custom roles are Enterprise only).
pub const PERMISSIONS: &[&str] = &["pull", "triage", "push", "maintain", "admin"];

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

    /// Before changing the constant tables, record the observed evidence first.
    #[test]
    fn repository_roles_are_pinned() {
        assert_eq!(
            REPOSITORY_ROLES,
            &[(2, "maintain"), (4, "write"), (5, "admin")]
        );
    }

    #[test]
    fn actor_types_round_trip() {
        for (api, toml) in ACTOR_TYPES {
            assert_eq!(actor_type_to_toml(api), Some(*toml));
            assert_eq!(actor_type_to_api(toml), Some(*api));
        }
    }
}
