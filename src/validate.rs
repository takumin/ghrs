//! Validation of the declaration file. Errors are collected and returned together rather
//! than stopping at the first.

use serde_json::{Value as Json, json};
use std::collections::HashSet;

use crate::diagnostic::{Applicability, Code, Diagnostic, Diagnostics, Edit};
use crate::model::{BypassActor, Config, ReviewActor, Rule, Ruleset};
use crate::plan::Scope;
use crate::schema::{
    ACTORS_WITHOUT_ID, ALLOWED_ACTIONS, BRANCH_ONLY_RULES, BYPASS_MODES, ENFORCEMENTS,
    FORK_PR_APPROVAL_POLICIES, MAX_RULESETS, ORG_ONLY_RULES, PERMISSIONS, PUSH_RULES,
    REPOSITORY_ROLES, ROLES_WITHOUT_WRITE, RULE_NAMES, TAG_ONLY_RULES, TARGETS, VISIBILITIES,
    WORKFLOW_PERMISSIONS, actor_type_names, actor_type_to_api, ref_patterns, role_id, rule_to_api,
};
use crate::{document, explain};

fn allowed(values: &[&str]) -> String {
    format!("allowed values: {}", values.join(" / "))
}

/// Path of a keyed entry such as `ruleset.main`. A key that is not a bare TOML key is quoted
/// (`ruleset."release/*"`).
pub fn entry_path(table: &str, key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        format!("{table}.{key}")
    } else {
        format!("{table}.{}", serde_json::Value::from(key))
    }
}

fn remove(path: &str) -> Edit {
    Edit::Remove {
        path: path.to_string(),
    }
}

/// Parse the declaration file. Unlike validation, parsing stops at the first error.
pub fn parse(text: &str) -> Result<Config, Diagnostics> {
    let one = |d| Diagnostics::new(vec![d]);
    let de = toml::Deserializer::parse(text).map_err(|err| {
        one(with_location(
            Diagnostic::error(Code::InvalidToml, err.message()),
            err.span(),
            text,
        ))
    })?;
    serde_path_to_error::deserialize(de).map_err(|err| one(deserialize_error(err, text)))
}

fn deserialize_error(err: serde_path_to_error::Error<toml::de::Error>, text: &str) -> Diagnostic {
    use serde_path_to_error::Segment;
    let path = key_path(err.path());
    // Taken from the segments rather than the rendered path, where a quoted key may hold a `.`
    let keys: Vec<String> = err
        .path()
        .iter()
        .map(|segment| match segment {
            Segment::Map { key } | Segment::Enum { variant: key } => key.clone(),
            _ => String::new(),
        })
        .collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    let err = err.into_inner();
    let message = err.message();
    let Some(expected) = message.strip_prefix("unknown field `").map(expected_keys) else {
        let d = with_location(
            Diagnostic::error(Code::InvalidValue, message),
            err.span(),
            text,
        );
        return match path {
            Some(path) => d.at(path),
            None => d,
        };
    };
    let path = path.unwrap_or_default();
    let key = keys.last().copied().unwrap_or_default();
    if let ["ruleset", _, "rules", key] = keys[..]
        && PUSH_RULES.contains(&key)
    {
        return Diagnostic::error(
            Code::PushRule,
            format!("{key} is a push ruleset rule and cannot be used in branch or tag rulesets"),
        )
        .at(&path)
        .help("push rulesets are not available on the free plan")
        .fix(Applicability::MaybeIncorrect, remove(&path));
    }
    let d = Diagnostic::error(Code::UnknownKey, format!("unknown key `{key}`")).at(&path);
    if let Some(to) = renamed(&keys) {
        return d.help(format!("`{key}` is written `{to}`")).fix(
            Applicability::MachineApplicable,
            Edit::Rename {
                path: path.clone(),
                to: to.to_string(),
            },
        );
    }
    match similar(key, &expected) {
        Some(to) => d.help(format!("did you mean `{to}`?")).fix(
            Applicability::MaybeIncorrect,
            Edit::Rename {
                path: path.clone(),
                to: to.to_string(),
            },
        ),
        None if expected.is_empty() => d.help("this table takes no keys"),
        None => d.help(format!("allowed keys: {}", expected.join(" / "))),
    }
}

/// The name of a key that ghrs writes differently from the API, or wrote differently before,
/// when `keys` is written by that other name. `similar` would not find it
/// (`deletion` -> `restrict_deletions`).
fn renamed(keys: &[&str]) -> Option<&'static str> {
    match keys {
        ["ruleset", _, "rules", rule] => RULE_NAMES
            .iter()
            .find(|(api, _)| api == rule)
            .map(|(_, toml)| *toml),
        [
            "ruleset",
            _,
            "rules",
            "required_status_checks",
            "strict" | "strict_required_status_checks_policy",
        ] => Some("require_up_to_date"),
        _ => None,
    }
}

/// Set where `span` starts as the location of `d`.
fn with_location(d: Diagnostic, span: Option<std::ops::Range<usize>>, text: &str) -> Diagnostic {
    match span {
        Some(span) => d.located(document::location(text, span.start)),
        None => d,
    }
}

/// Render a deserialization path the way diagnostics write paths (`ruleset."release/*".rules`).
fn key_path(path: &serde_path_to_error::Path) -> Option<String> {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for segment in path.iter() {
        match segment {
            Segment::Map { key } if out.is_empty() => out = entry_path("", key)[1..].to_string(),
            Segment::Map { key } => out = entry_path(&out, key),
            Segment::Seq { index } => out.push_str(&format!("[{index}]")),
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The keys listed in serde's "unknown field `x`, expected one of `a`, `b`" message. serde
/// offers no structured access to them.
fn expected_keys(rest: &str) -> Vec<&str> {
    let Some((_, expected)) = rest.split_once(", expected ") else {
        return Vec::new();
    };
    expected.split('`').skip(1).step_by(2).collect()
}

/// The allowed key `key` was most likely meant to be. Like rustc, accept an edit distance up to a
/// third of the key's length, or a key that `key` is the only prefix of
/// (`allow_rebase` -> `allow_rebase_merge`).
fn similar<'a>(key: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let limit = (key.len() / 3).max(1);
    let closest = candidates
        .iter()
        .map(|c| (distance(key, c), *c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c);
    if closest.is_some() {
        return closest;
    }
    let mut prefixed = candidates.iter().filter(|c| c.starts_with(key));
    match (prefixed.next(), prefixed.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Levenshtein distance.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (prev + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Validation that does not depend on GitHub state. Runs before any API call.
///
/// `app_auth` is whether to authenticate as a GitHub App. `integration = "self"` can only be
/// resolved with App authentication.
pub fn check(config: &Config, app_auth: bool) -> Vec<Diagnostic> {
    let mut errors = Vec::new();

    // Each is one segment of the API route (`/repos/{owner}/{repo}`)
    for (key, value) in [("owner", &config.meta.owner), ("repo", &config.meta.repo)] {
        if let Some(value) = value
            && (value.is_empty() || value.contains(['/', ' ', '?', '#']))
        {
            errors.push(
                Diagnostic::error(
                    Code::InvalidMeta,
                    format!("meta.{key} \"{value}\" is not a valid {key} name"),
                )
                .at(format!("meta.{key}"))
                .help("write the owner and the repository name separately, as in owner/repo"),
            );
        }
    }
    for (i, entry) in config.meta.unmanaged.iter().enumerate() {
        if let Err(err) = explain::check_key(entry) {
            errors.push(
                Diagnostic::error(
                    Code::UnknownUnmanaged,
                    format!("meta.unmanaged entry \"{entry}\" is not a key path"),
                )
                .at(format!("meta.unmanaged[{i}]"))
                .help(err.to_string()),
            );
        }
    }

    if let Some(visibility) = &config.repository.visibility
        && !VISIBILITIES.contains(&visibility.as_str())
    {
        errors.push(
            Diagnostic::error(
                Code::VisibilityNotAllowed,
                format!("repository.visibility \"{visibility}\" is not allowed"),
            )
            .at("repository.visibility")
            .help(allowed(VISIBILITIES)),
        );
    }

    check_actions(config, &mut errors);

    if config.rulesets.len() > MAX_RULESETS {
        errors.push(
            Diagnostic::error(
                Code::TooManyRulesets,
                format!(
                    "at most {MAX_RULESETS} rulesets are allowed per repository ({} declared)",
                    config.rulesets.len()
                ),
            )
            .at("ruleset"),
        );
    }

    for ruleset in &config.rulesets {
        check_ruleset(
            &entry_path("ruleset", &ruleset.name),
            ruleset,
            app_auth,
            &mut errors,
        );
    }

    let mut logins = HashSet::new();
    for c in &config.collaborators {
        let path = entry_path("collaborator", &c.login);
        if c.login.is_empty() {
            errors
                .push(Diagnostic::error(Code::EmptyLogin, "collaborator login is empty").at(&path));
        }
        // GitHub logins are case-insensitive
        if !logins.insert(c.login.to_ascii_lowercase()) {
            errors.push(
                Diagnostic::error(
                    Code::DuplicateCollaborator,
                    format!("collaborator \"{}\" is declared more than once", c.login),
                )
                .at(&path)
                .help("logins are case-insensitive; keep one entry per user"),
            );
        }
        check_permission(&path, "collaborator", &c.login, &c.permission, &mut errors);
    }

    let mut slugs = HashSet::new();
    for t in &config.teams {
        let path = entry_path("team", &t.slug);
        if t.slug.is_empty() {
            errors.push(Diagnostic::error(Code::EmptySlug, "team slug is empty").at(&path));
        }
        if !slugs.insert(t.slug.to_ascii_lowercase()) {
            errors.push(
                Diagnostic::error(
                    Code::DuplicateTeam,
                    format!("team \"{}\" is declared more than once", t.slug),
                )
                .at(&path)
                .help("slugs are case-insensitive; keep one entry per team"),
            );
        }
        check_permission(&path, "team", &t.slug, &t.permission, &mut errors);
    }

    errors
}

fn check_actions(config: &Config, errors: &mut Vec<Diagnostic>) {
    let actions = &config.actions;
    for (key, value, values, code) in [
        (
            "allowed_actions",
            &actions.allowed_actions,
            ALLOWED_ACTIONS,
            Code::AllowedActionsNotAllowed,
        ),
        (
            "default_workflow_permissions",
            &actions.default_workflow_permissions,
            WORKFLOW_PERMISSIONS,
            Code::WorkflowPermissionsNotAllowed,
        ),
        (
            "fork_pr_contributor_approval",
            &actions.fork_pr_contributor_approval,
            FORK_PR_APPROVAL_POLICIES,
            Code::ForkPrApprovalNotAllowed,
        ),
    ] {
        if let Some(value) = value
            && !values.contains(&value.as_str())
        {
            errors.push(
                Diagnostic::error(code, format!("actions.{key} \"{value}\" is not allowed"))
                    .at(format!("actions.{key}"))
                    .help(allowed(values)),
            );
        }
    }

    // GitHub rejects selected_actions with 409 under any other allowed_actions (observed: GET).
    // Relying on the current value instead would make the declaration's meaning depend on it
    // An unknown allowed_actions is reported above, with its allowed values
    let known = actions
        .allowed_actions
        .as_deref()
        .is_none_or(|value| ALLOWED_ACTIONS.contains(&value));
    if actions.selected_actions.is_some()
        && known
        && actions.allowed_actions.as_deref() != Some("selected")
    {
        let path = "actions.allowed_actions";
        let d = Diagnostic::error(
            Code::SelectedActionsNeedSelected,
            "actions.selected_actions needs actions.allowed_actions = \"selected\"",
        );
        errors.push(match &actions.allowed_actions {
            // It may change the policy GitHub has now, so it is not machine-applicable
            None => d.at(path).help("declare the policy it belongs to").fix(
                Applicability::MaybeIncorrect,
                Edit::Set {
                    path: path.to_string(),
                    value: json!("selected"),
                },
            ),
            Some(_) => d
                .at(path)
                .help("set allowed_actions to \"selected\", or remove actions.selected_actions"),
        });
    }

    // GitHub ignores include_claim_keys under the default template and does not return it, so
    // plan would show a diff every time
    if let Some(oidc) = &actions.oidc_subject_claim
        && oidc.include_claim_keys.is_some()
        && oidc.use_default != Some(false)
    {
        let path = "actions.oidc_subject_claim.use_default";
        let d = Diagnostic::error(
            Code::ClaimKeysNeedCustomTemplate,
            "actions.oidc_subject_claim.include_claim_keys needs \
            actions.oidc_subject_claim.use_default = false",
        )
        .at(path);
        errors.push(match oidc.use_default {
            // It may change the template GitHub has now, so it is not machine-applicable
            None => d.help("declare the template it belongs to").fix(
                Applicability::MaybeIncorrect,
                Edit::Set {
                    path: path.to_string(),
                    value: json!(false),
                },
            ),
            Some(_) => d.help("set use_default to false, or remove include_claim_keys"),
        });
    }
}

fn check_permission(
    path: &str,
    kind: &str,
    key: &str,
    permission: &str,
    errors: &mut Vec<Diagnostic>,
) {
    if !PERMISSIONS.contains(&permission) {
        errors.push(
            Diagnostic::error(
                Code::PermissionNotAllowed,
                format!("{kind} \"{key}\": permission \"{permission}\" is not allowed"),
            )
            .at(format!("{path}.permission"))
            .help(allowed(PERMISSIONS)),
        );
    }
}

/// Validation performed once the target repository's owner and current visibility are known.
/// Runs before name resolution so API calls for resolution are not wasted.
///
/// `owner` is `None` when it is not known, which skips the checks that compare logins with it.
pub fn check_context(
    config: &Config,
    owner: Option<&str>,
    owned_by_org: bool,
    current_visibility: &str,
    scope: &Scope,
) -> Vec<Diagnostic> {
    let mut errors = Vec::new();

    let collaborators = if scope.collaborators {
        &config.collaborators[..]
    } else {
        &[]
    };
    for c in collaborators {
        let path = entry_path("collaborator", &c.login);
        if owner.is_some_and(|owner| c.login.eq_ignore_ascii_case(owner)) {
            // The owner is always admin, so removing it changes nothing on the GitHub side
            errors.push(
                Diagnostic::error(
                    Code::OwnerAsCollaborator,
                    format!(
                        "collaborator \"{}\" is the repository owner and cannot be declared as a \
                        collaborator",
                        c.login
                    ),
                )
                .at(&path)
                .help("the owner always has admin access; remove this entry")
                .fix(Applicability::MachineApplicable, remove(&path)),
            );
        }
        // Collaborators on a personal repository get push only. Specifying admin makes GitHub
        // return 422 (Cannot assign ... permission of admin) (observed)
        if !owned_by_org && c.permission != "push" {
            let permission = format!("{path}.permission");
            errors.push(
                Diagnostic::error(
                    Code::PersonalCollaboratorPermission,
                    format!(
                        "collaborator \"{}\": collaborators on a personal repository can only have \
                        permission push",
                        c.login
                    ),
                )
                .at(&permission)
                .help("personal repositories have no other collaborator permission levels")
                .fix(
                    Applicability::MaybeIncorrect,
                    Edit::Set {
                        path: permission,
                        value: json!("push"),
                    },
                ),
            );
        }
    }
    if !owned_by_org && scope.teams && !config.teams.is_empty() {
        errors.push(
            Diagnostic::error(
                Code::TeamOnPersonal,
                "team is only available on Organization-owned repositories",
            )
            .at("team")
            .help("teams exist only in Organizations; remove the [team.*] tables"),
        );
    }
    if !owned_by_org && scope.rulesets {
        for ruleset in &config.rulesets {
            let base = entry_path("ruleset", &ruleset.name);
            for (rule, value) in &ruleset.rules.entries() {
                if ORG_ONLY_RULES.contains(&rule_to_api(rule)) && value.as_bool() != Some(false) {
                    let path = format!("{base}.rules.{rule}");
                    errors.push(
                        Diagnostic::error(
                            Code::OrgOnlyRule,
                            format!(
                                "ruleset \"{}\": rules.{rule} is only available on \
                                Organization-owned repositories",
                                ruleset.name
                            ),
                        )
                        .at(&path)
                        .fix(Applicability::MaybeIncorrect, remove(&path)),
                    );
                }
            }
        }
    }

    if !owned_by_org && scope.rulesets {
        for ruleset in &config.rulesets {
            let base = entry_path("ruleset", &ruleset.name);
            for (j, actor) in ruleset.bypass_actors.iter().enumerate() {
                let path = format!("{base}.bypass_actors[{j}]");
                if actor.actor_type == "team" {
                    errors.push(
                        Diagnostic::error(
                            Code::TeamActorOnPersonal,
                            format!(
                                "ruleset \"{}\": type = \"team\" is only available on \
                                Organization-owned repositories",
                                ruleset.name
                            ),
                        )
                        .at(&path),
                    );
                }
                if actor.actor_type == "organization_admin" {
                    errors.push(
                        organization_admin_on_personal(&ruleset.name)
                            .at(&path)
                            .help(
                                "it is left over from older GitHub behavior and has no effect; \
                                remove it from the declaration and apply deletes it from GitHub",
                            )
                            .fix(Applicability::MachineApplicable, remove(&path)),
                    );
                }
            }
            if let Some(Rule::Params(pr)) = &ruleset.rules.pull_request {
                // Accepted on personal repositories but not saved, so plan shows a diff every time
                // (observed)
                if pr.dismissal_restriction.is_some() {
                    let path = format!("{base}.rules.pull_request.dismissal_restriction");
                    errors.push(
                        Diagnostic::error(
                            Code::DismissalRestrictionOnPersonal,
                            format!(
                                "ruleset \"{}\": rules.pull_request.dismissal_restriction is only \
                                available on Organization-owned repositories",
                                ruleset.name
                            ),
                        )
                        .at(&path)
                        .help("GitHub accepts it on personal repositories but does not store it")
                        .fix(Applicability::MachineApplicable, remove(&path)),
                    );
                }
                for (path, actor) in pr.review_actors() {
                    // The error above already removes the whole dismissal_restriction
                    if actor.actor_type == "team" && !path.starts_with("dismissal_restriction") {
                        errors.push(
                            Diagnostic::error(
                                Code::TeamActorOnPersonal,
                                format!(
                                    "ruleset \"{}\": type = \"team\" in rules.pull_request.{path} \
                                    is only available on Organization-owned repositories",
                                    ruleset.name
                                ),
                            )
                            .at(format!("{base}.rules.pull_request.{path}")),
                        );
                    }
                }
            }
        }
    }

    // Rulesets are unavailable for private repositories on the free plan
    let declared_visibility = config
        .repository
        .visibility
        .as_deref()
        .filter(|_| scope.repository);
    let visibility = visibility(config, current_visibility, scope);
    if scope.rulesets && visibility == "private" && !config.rulesets.is_empty() {
        let mut d = Diagnostic::error(
            Code::RulesetsOnPrivate,
            "rulesets are not available on private repositories on the GitHub Free plan, which \
            ghrs assumes",
        )
        .help("set repository.visibility to \"public\" or remove the rulesets");
        if declared_visibility.is_some() {
            d = d.at("repository.visibility");
        }
        errors.push(d);
    }

    // GitHub returns 422 (`Fork PR approval is not allowed for private repositories.`)
    // (observed)
    if scope.actions
        && visibility == "private"
        && config.actions.fork_pr_contributor_approval.is_some()
    {
        let path = "actions.fork_pr_contributor_approval";
        let mut d = Diagnostic::error(
            Code::ForkPrApprovalOnPrivate,
            "actions.fork_pr_contributor_approval is only available on public repositories",
        )
        .at(path)
        .fix(Applicability::MaybeIncorrect, remove(path));
        if declared_visibility.is_some() {
            d = d.help("set repository.visibility to \"public\" or remove it");
        }
        errors.push(d);
    }

    errors
}

/// The visibility the repository has once applied. A declared visibility is applied only with
/// the repository in scope.
pub fn visibility<'a>(config: &'a Config, current: &'a str, scope: &Scope) -> &'a str {
    config
        .repository
        .visibility
        .as_deref()
        .filter(|_| scope.repository)
        .unwrap_or(current)
}

/// Adding `organization_admin` to a personal repository's ruleset makes GitHub return 422
/// (`ruleset source must be in an organization`) (observed). It cannot be set in the Web UI
/// either. Older repositories may still have it as a leftover of a previous specification,
/// but personal repositories have no Organization admin, so it has no effect. Rather than
/// leave it declarable, make the user remove it.
pub fn organization_admin_on_personal(ruleset: &str) -> Diagnostic {
    Diagnostic::error(
        Code::OrganizationAdminOnPersonal,
        format!(
            "ruleset \"{ruleset}\": organization_admin cannot be used in rulesets of a personal \
            repository"
        ),
    )
}

fn check_ruleset(base: &str, ruleset: &Ruleset, app_auth: bool, errors: &mut Vec<Diagnostic>) {
    let at = format!("ruleset \"{}\"", ruleset.name);

    if ruleset.name.is_empty() {
        errors.push(Diagnostic::error(Code::EmptyRulesetName, "ruleset name is empty").at(base));
    }
    if !TARGETS.contains(&ruleset.target.as_str()) {
        errors.push(
            Diagnostic::error(
                Code::TargetNotAllowed,
                format!("{at}: target \"{}\" is not allowed", ruleset.target),
            )
            .at(format!("{base}.target"))
            .help(allowed(TARGETS)),
        );
    }
    if ruleset.enforcement == "evaluate" {
        // evaluate only records violations without blocking. The closest non-blocking mode is
        // disabled
        let path = format!("{base}.enforcement");
        errors.push(
            Diagnostic::error(
                Code::EvaluateEnforcement,
                format!(
                    "{at}: enforcement = \"evaluate\" is not allowed because it is \
                    Enterprise-only"
                ),
            )
            .at(&path)
            .help(
                "use \"active\" to enforce the ruleset, or \"disabled\" to keep it without \
                enforcing",
            )
            .fix(
                Applicability::MaybeIncorrect,
                Edit::Set {
                    path,
                    value: json!("disabled"),
                },
            ),
        );
    } else if !ENFORCEMENTS.contains(&ruleset.enforcement.as_str()) {
        errors.push(
            Diagnostic::error(
                Code::EnforcementNotAllowed,
                format!(
                    "{at}: enforcement \"{}\" is not allowed",
                    ruleset.enforcement
                ),
            )
            .at(format!("{base}.enforcement"))
            .help(allowed(ENFORCEMENTS)),
        );
    }

    if let Some((prefix, specials)) = ref_patterns(&ruleset.target) {
        let conditions = &ruleset.conditions;
        for (key, patterns) in [
            ("include", &conditions.include),
            ("exclude", &conditions.exclude),
        ] {
            for (k, pattern) in patterns.iter().enumerate() {
                if specials.contains(&pattern.as_str()) || pattern.starts_with(prefix) {
                    continue;
                }
                let path = format!("{base}.conditions.{key}[{k}]");
                let mut d = Diagnostic::error(
                    Code::RefPatternPrefix,
                    format!(
                        "{at}: \"{pattern}\" in conditions.{key} is not allowed with target = \
                        \"{}\"",
                        ruleset.target
                    ),
                )
                .at(&path)
                .help(format!(
                    "start it with {prefix}, or use one of {}",
                    specials.join(" / ")
                ));
                // Prefixing another kind of ref (refs/tags/... etc.) makes no sense
                if !pattern.starts_with("refs/") && !pattern.starts_with('~') {
                    d = d.fix(
                        Applicability::MaybeIncorrect,
                        Edit::Set {
                            path,
                            value: json!(format!("{prefix}{pattern}")),
                        },
                    );
                }
                errors.push(d);
            }
        }
    }

    for (j, actor) in ruleset.bypass_actors.iter().enumerate() {
        let path = format!("{base}.bypass_actors[{j}]");
        check_actor(&at, &path, &ruleset.target, actor, app_auth, errors);
    }

    for (rule, value) in &ruleset.rules.entries() {
        let path = format!("{base}.rules.{rule}");
        find_id_keys(&path, value, &mut |found| {
            errors.push(
                Diagnostic::error(
                    Code::NumericIdKey,
                    format!(
                        "{at}: {} is keyed by numeric IDs",
                        found.trim_start_matches(&format!("{base}."))
                    ),
                )
                .at(found)
                .help("write it in a form that uses names"),
            );
        });
        if value.as_bool() == Some(false) {
            continue;
        }
        let only = if BRANCH_ONLY_RULES.contains(&rule_to_api(rule)) {
            Some("branch")
        } else if TAG_ONLY_RULES.contains(&rule_to_api(rule)) {
            Some("tag")
        } else {
            None
        };
        if let Some(only) = only
            && ruleset.target != only
        {
            errors.push(
                Diagnostic::error(
                    Code::RuleTargetMismatch,
                    format!(
                        "{at}: rules.{rule} is only available in rulesets with target = \"{only}\""
                    ),
                )
                .at(&path)
                .help(format!(
                    "move the rule to a ruleset with target = \"{only}\", or remove it"
                )),
            );
        }
    }

    if let Some(Rule::Params(pr)) = &ruleset.rules.pull_request {
        for (path, actor) in pr.review_actors() {
            // required_reviewers[].reviewer accepts only teams (observed)
            let kinds: &[&str] = if path.starts_with("required_reviewers") {
                &["team"]
            } else {
                &["team", "repository_role", "user", "integration"]
            };
            let path = format!("rules.pull_request.{path}");
            let full = format!("{base}.{path}");
            check_review_actor(&at, &path, &full, actor, kinds, app_auth, errors);
        }
    }
}

fn check_actor(
    at: &str,
    path: &str,
    target: &str,
    actor: &BypassActor,
    app_auth: bool,
    errors: &mut Vec<Diagnostic>,
) {
    let kind = actor.actor_type.as_str();
    if actor_type_to_api(kind).is_none() {
        let types = actor_type_names();
        errors.push(
            Diagnostic::error(
                Code::UnknownActorType,
                format!("{at}: unknown bypass actor type \"{kind}\""),
            )
            .at(format!("{path}.type"))
            .help(allowed(&types)),
        );
        return;
    }

    let mode = format!("{path}.mode");
    if !BYPASS_MODES.contains(&actor.mode.as_str()) {
        errors.push(
            Diagnostic::error(
                Code::BypassModeNotAllowed,
                format!("{at}: bypass actor mode \"{}\" is not allowed", actor.mode),
            )
            .at(&mode)
            .help(allowed(BYPASS_MODES)),
        );
    }
    // Do not suggest a fix that changes the mode. Changing to always widens the scope of the bypass
    if actor.mode == "pull_request" && target != "branch" {
        errors.push(
            Diagnostic::error(
                Code::PullRequestModeTarget,
                format!(
                    "{at}: mode = \"pull_request\" is only available in rulesets with target = \
                    \"branch\""
                ),
            )
            .at(&mode),
        );
    }
    if actor.mode == "pull_request" && kind == "deploy_key" {
        errors.push(
            Diagnostic::error(
                Code::DeployKeyPullRequestMode,
                format!("{at}: deploy_key cannot use mode = \"pull_request\""),
            )
            .at(&mode)
            .help("deploy keys push directly and cannot open pull requests"),
        );
    }

    let name = format!("{path}.name");
    let d = match (kind, actor.name.as_deref()) {
        (kind, Some(_)) if ACTORS_WITHOUT_ID.contains(&kind) => Diagnostic::error(
            Code::ActorTakesNoName,
            format!("{at}: {kind} refers to every actor of its kind, so it takes no name"),
        )
        .at(&name)
        .fix(Applicability::MachineApplicable, remove(&name)),
        (kind, None) if ACTORS_WITHOUT_ID.contains(&kind) => return,
        (_, None) => Diagnostic::error(
            Code::ActorMissingName,
            format!("{at}: {kind} bypass actor has no name"),
        )
        .at(path)
        .help("add name = \"...\""),
        (_, Some(value)) if value.trim().is_empty() => Diagnostic::error(
            Code::EmptyActorName,
            format!("{at}: {kind} bypass actor has an empty name"),
        )
        .at(&name),
        ("integration", Some("self")) if !app_auth => Diagnostic::error(
            Code::SelfNeedsApp,
            format!(
                "{at}: name = \"self\" is only available with GitHub App authentication (it cannot \
                be resolved with a PAT)"
            ),
        )
        .at(&name)
        .help("authenticate as a GitHub App (--app-id / GHRS_APP_ID), or write the App slug"),
        // Only private App integrations may be written as numeric IDs
        ("integration", Some(_)) => return,
        // A team slug can be all digits (a team named "2024"), so only roles are checked here;
        // resolution reports a team that is not found
        ("repository_role", Some(value)) if is_numeric(value) => Diagnostic::error(
            Code::NumericActorName,
            format!("{at}: {kind} name cannot be a numeric ID \"{value}\""),
        )
        .at(&name)
        .help(format!(
            "specify the {kind} by name; only integration accepts a numeric App ID"
        )),
        ("repository_role", Some(role)) if ROLES_WITHOUT_WRITE.contains(&role) => {
            Diagnostic::error(
                Code::RoleWithoutWrite,
                format!(
                    "{at}: repository_role \"{role}\" has no write access and cannot be a bypass \
                    actor"
                ),
            )
            .at(&name)
            .help(allowed(&roles()))
        }
        ("repository_role", Some(role)) if role_id(role).is_none() => Diagnostic::error(
            Code::UnknownRole,
            format!("{at}: repository_role \"{role}\" is not a built-in role"),
        )
        .at(&name)
        .help(allowed(&roles())),
        _ => return,
    };
    errors.push(d);
}

fn roles() -> Vec<&'static str> {
    REPOSITORY_ROLES.iter().map(|(_, name)| *name).collect()
}

/// `path` is the position within the ruleset (for messages); `full` is the position in the
/// declaration file.
fn check_review_actor(
    at: &str,
    path: &str,
    full: &str,
    actor: &ReviewActor,
    kinds: &[&str],
    app_auth: bool,
    errors: &mut Vec<Diagnostic>,
) {
    let (kind, name) = (actor.actor_type.as_str(), actor.name.as_str());
    if !kinds.contains(&kind) {
        errors.push(
            Diagnostic::error(
                Code::ReviewActorTypeNotAllowed,
                format!("{at}: type \"{kind}\" in {path} is not allowed"),
            )
            .at(format!("{full}.type"))
            .help(allowed(kinds)),
        );
    } else if name.trim().is_empty() {
        errors.push(
            Diagnostic::error(
                Code::EmptyActorName,
                format!("{at}: name in {path} is empty"),
            )
            .at(format!("{full}.name")),
        );
    } else if kind == "integration" && is_numeric(name) {
        // A team slug or a user login can be all digits, so only App slugs are checked here;
        // resolution reports a team or user that is not found
        errors.push(
            Diagnostic::error(
                Code::NumericActorName,
                format!("{at}: name in {path} cannot be a numeric ID \"{name}\""),
            )
            .at(format!("{full}.name"))
            .help(format!("specify the {kind} by name")),
        );
    } else if kind == "integration" && name == "self" && !app_auth {
        errors.push(
            Diagnostic::error(
                Code::SelfNeedsApp,
                format!(
                    "{at}: name = \"self\" in {path} is only available with GitHub App \
                    authentication (it cannot be resolved with a PAT)"
                ),
            )
            .at(format!("{full}.name"))
            .help("authenticate as a GitHub App (--app-id / GHRS_APP_ID), or write the App slug"),
        );
    } else if kind == "repository_role" && name != "admin" {
        // GitHub rejects anything other than admin with 422 (Only the Repository admin role is
        // allowed in the dismissal restriction list) (observed)
        errors.push(
            Diagnostic::error(
                Code::DismissalRoleNotAdmin,
                format!("{at}: only admin is allowed as repository_role in {path}"),
            )
            .at(format!("{full}.name")),
        );
    }
}

fn find_id_keys(path: &str, value: &Json, found: &mut impl FnMut(String)) {
    match value {
        Json::Object(table) => {
            for (key, value) in table {
                let path = format!("{path}.{key}");
                if key == "id" || key.ends_with("_id") || key.ends_with("_ids") {
                    found(path.clone());
                }
                find_id_keys(&path, value, found);
            }
        }
        Json::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                find_id_keys(&format!("{path}[{i}]"), item, found);
            }
        }
        _ => {}
    }
}

fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Fix;

    const ALL: Scope = Scope {
        repository: true,
        actions: true,
        rulesets: true,
        collaborators: true,
        teams: true,
    };
    const ACCESS: Scope = Scope {
        repository: false,
        actions: false,
        rulesets: false,
        collaborators: true,
        teams: true,
    };
    const REPOSITORY: Scope = Scope {
        repository: true,
        actions: true,
        rulesets: false,
        collaborators: false,
        teams: false,
    };

    fn errors(toml: &str) -> Vec<Diagnostic> {
        let config: Config = toml::from_str(toml).unwrap();
        check(&config, false)
    }

    fn ruleset(body: &str) -> String {
        format!("[ruleset.r]\ntarget = \"branch\"\nenforcement = \"active\"\n{body}")
    }

    #[test]
    fn accepts_spec_sample() {
        let toml = ruleset(
            r#"
[[ruleset.r.bypass_actors]]
type = "integration"
name = "renovate"
mode = "pull_request"

[[ruleset.r.bypass_actors]]
type = "organization_admin"

[ruleset.r.rules]
restrict_deletions = true
restrict_updates = true

[ruleset.r.rules.required_status_checks]
require_up_to_date = true
checks = [{ context = "test" }, { context = "build", integration = "github-actions" }]
"#,
        );
        let errors = errors(&toml);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("[repository]\nhas_issue = true").is_err());
    }

    fn parse_error(toml: &str) -> Diagnostic {
        let mut errors = parse(toml).err().expect("parses").diagnostics;
        assert_eq!(errors.len(), 1);
        errors.remove(0)
    }

    fn rename(d: &Diagnostic) -> Option<(&str, &str)> {
        match d.fix.as_ref().map(|f| &f.edit) {
            Some(Edit::Rename { path, to }) => Some((path, to)),
            _ => None,
        }
    }

    #[test]
    fn suggests_the_key_that_was_meant() {
        let d = parse_error("[repository]\nallow_rebase = false");
        assert_eq!(d.code, Code::UnknownKey);
        assert_eq!(
            rename(&d),
            Some(("repository.allow_rebase", "allow_rebase_merge"))
        );

        let d = parse_error(&ruleset("[ruleset.r.rules]\nblock_force_push = true"));
        assert_eq!(
            rename(&d),
            Some(("ruleset.r.rules.block_force_push", "block_force_pushes"))
        );

        let d = parse_error(&ruleset(
            "[ruleset.r.rules.pull_request]\nrequired_approving_review_cnt = 1",
        ));
        assert_eq!(
            rename(&d),
            Some((
                "ruleset.r.rules.pull_request.required_approving_review_cnt",
                "required_approving_review_count"
            ))
        );
        assert_eq!(d.fix.unwrap().applicability, Applicability::MaybeIncorrect);
    }

    /// The API's names, and the names ghrs used before, are renamed without guessing
    #[test]
    fn renames_keys_written_by_another_name() {
        for (body, path, to) in [
            (
                "[ruleset.r.rules]\ndeletion = true",
                "ruleset.r.rules.deletion",
                "restrict_deletions",
            ),
            (
                "[ruleset.r.rules]\nnon_fast_forward = true",
                "ruleset.r.rules.non_fast_forward",
                "block_force_pushes",
            ),
            (
                "[ruleset.r.rules.required_status_checks]\nstrict = true",
                "ruleset.r.rules.required_status_checks.strict",
                "require_up_to_date",
            ),
            (
                "[ruleset.r.rules.required_status_checks]\n\
                strict_required_status_checks_policy = true",
                "ruleset.r.rules.required_status_checks.strict_required_status_checks_policy",
                "require_up_to_date",
            ),
        ] {
            let d = parse_error(&ruleset(body));
            assert_eq!(d.code, Code::UnknownKey);
            assert_eq!(rename(&d), Some((path, to)), "{body}");
            assert_eq!(
                d.fix.unwrap().applicability,
                Applicability::MachineApplicable
            );
        }
    }

    #[test]
    fn rule_diagnostics_refer_to_the_rest_api_description() {
        let reference = |toml: &str| parse_error(toml).reference;
        let schema = |name: &str| {
            Some(format!(
                "{}#/components/schemas/{name}",
                crate::schema::REST_API_DESCRIPTION
            ))
        };
        assert_eq!(
            reference(&ruleset(
                "[ruleset.r.rules.pull_request]\nrequired_approving_review_cnt = 1"
            )),
            schema("repository-rule-pull-request")
        );
        // An unknown rule refers to the list of all rules
        assert_eq!(
            reference(&ruleset("[ruleset.r.rules]\nblock_force_push = true")),
            schema("repository-rule")
        );
        assert_eq!(reference("[repository]\nallow_rebase = false"), None);

        let toml = ruleset("[ruleset.r.rules.tag_name_pattern]\noperator = \"starts_with\"");
        let errors = errors(&toml);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].reference,
            schema("repository-rule-tag-name-pattern")
        );
    }

    #[test]
    fn lists_allowed_keys_when_nothing_is_close() {
        let d = parse_error("[collaborator.octocat]\nrole = \"push\"");
        assert_eq!(d.path.as_deref(), Some("collaborator.octocat.role"));
        assert!(d.fix.is_none());
        assert_eq!(d.help.as_deref(), Some("allowed keys: permission"));
    }

    #[test]
    fn quotes_keys_in_paths() {
        let toml = ruleset("foo = 1").replace("[ruleset.r]", "[ruleset.\"release/*\"]");
        let d = parse_error(&toml);
        assert_eq!(d.path.as_deref(), Some("ruleset.\"release/*\".foo"));
    }

    #[test]
    fn reports_values_of_the_wrong_type() {
        let d = parse_error(&ruleset("[ruleset.r.rules]\nrestrict_deletions = 1"));
        assert_eq!(d.code, Code::InvalidValue);
        assert_eq!(
            d.path.as_deref(),
            Some("ruleset.r.rules.restrict_deletions")
        );
        assert_eq!(d.location.map(|l| l.line), Some(5), "{d:?}");

        let d = parse_error(&ruleset("[ruleset.r.rules]\npull_request = \"yes\""));
        assert!(
            d.message.contains("a boolean or a table of parameters"),
            "{}",
            d.message
        );

        let d = parse_error("[ruleset.r]\ntarget = \"branch\"");
        assert_eq!(d.path.as_deref(), Some("ruleset.r"));
        assert!(d.message.contains("enforcement"), "{}", d.message);
    }

    #[test]
    fn reports_toml_syntax_errors_with_the_line() {
        let d = parse_error("[repository]\n[ruleset\n");
        assert_eq!(d.code, Code::InvalidToml);
        assert!(d.path.is_none());
        assert_eq!(d.location.map(|l| l.line), Some(2), "{d:?}");
    }

    #[test]
    fn similar_keys() {
        let keys = ["has_issues", "has_wiki", "has_projects"];
        assert_eq!(similar("has_issue", &keys), Some("has_issues"));
        assert_eq!(similar("has_wikki", &keys), Some("has_wiki"));
        // Ambiguous prefix
        assert_eq!(similar("has", &keys), None);
        assert_eq!(similar("visibility", &keys), None);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
    }

    #[test]
    fn rejects_evaluate() {
        let toml = ruleset("").replace("active", "evaluate");
        assert_eq!(errors(&toml).len(), 1);
    }

    #[test]
    fn rejects_too_many_rulesets() {
        let many: String = (0..=MAX_RULESETS)
            .map(|i| ruleset("").replace("[ruleset.r]", &format!("[ruleset.r{i}]")))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(errors(&many).len(), 1);
    }

    #[test]
    fn rejects_duplicate_names() {
        let toml = format!("{}\n{}", ruleset(""), ruleset(""));
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    #[test]
    fn rejects_array_of_tables() {
        let toml = "[[ruleset]]\nname = \"r\"\ntarget = \"branch\"\nenforcement = \"active\"";
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn quotes_keys_that_are_not_bare() {
        assert_eq!(entry_path("ruleset", "main"), "ruleset.main");
        assert_eq!(
            entry_path("ruleset", "release/v1"),
            "ruleset.\"release/v1\""
        );
        assert_eq!(entry_path("ruleset", ""), "ruleset.\"\"");
    }

    #[test]
    fn rejects_numeric_ids_except_integration() {
        let toml = ruleset(
            r#"
[[ruleset.r.bypass_actors]]
type = "integration"
name = "12345"

[[ruleset.r.bypass_actors]]
type = "repository_role"
name = "5"

# A team slug can be all digits
[[ruleset.r.bypass_actors]]
type = "team"
name = "678"

[ruleset.r.rules.workflows]
workflows = [{ repository_id = 1, path = "a.yml" }]
"#,
        );
        let errors = errors(&toml);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(errors[0].code, Code::NumericActorName);
        assert_eq!(
            errors[1].path.as_deref(),
            Some("ruleset.r.rules.workflows.workflows[0].repository_id")
        );
    }

    #[test]
    fn rejects_self_under_pat() {
        let toml = ruleset("[[ruleset.r.bypass_actors]]\ntype = \"integration\"\nname = \"self\"");
        assert_eq!(errors(&toml).len(), 1);
        let config: Config = toml::from_str(&toml).unwrap();
        assert!(check(&config, true).is_empty());
    }

    #[test]
    fn checks_actor_constraints() {
        let toml = ruleset(
            r#"
[[ruleset.r.bypass_actors]]
type = "organization_admin"
name = "x"

[[ruleset.r.bypass_actors]]
type = "deploy_key"
mode = "pull_request"

[[ruleset.r.bypass_actors]]
type = "deploy_key"
name = "ci"

[[ruleset.r.bypass_actors]]
type = "repository_role"
name = "owner"
"#,
        );
        assert_eq!(errors(&toml).len(), 4);

        let tag = ruleset(
            "[[ruleset.r.bypass_actors]]\ntype = \"organization_admin\"\nmode = \"pull_request\"",
        )
        .replace("branch", "tag");
        assert_eq!(errors(&tag).len(), 1);
    }

    #[test]
    fn rejects_team_on_personal_repository() {
        let toml = ruleset("[[ruleset.r.bypass_actors]]\ntype = \"team\"\nname = \"maintainers\"");
        let config: Config = toml::from_str(&toml).unwrap();
        assert_eq!(
            check_context(&config, Some("takumin"), false, "public", &ALL).len(),
            1
        );
        assert!(check_context(&config, Some("takumin"), true, "public", &ALL).is_empty());
    }

    #[test]
    fn rejects_organization_admin_on_personal_repository() {
        let toml = ruleset("[[ruleset.r.bypass_actors]]\ntype = \"organization_admin\"");
        let config: Config = toml::from_str(&toml).unwrap();
        assert_eq!(
            check_context(&config, Some("takumin"), false, "public", &ALL).len(),
            1
        );
        assert!(check_context(&config, Some("takumin"), true, "public", &ALL).is_empty());
    }

    #[test]
    fn rejects_empty_names() {
        let codes = |toml: &str| -> Vec<Code> {
            let config: Config = toml::from_str(toml).unwrap();
            check(&config, false).iter().map(|d| d.code).collect()
        };
        assert_eq!(
            codes("[team.\"\"]\npermission = \"push\""),
            [Code::EmptySlug]
        );
        assert_eq!(
            codes(&ruleset(
                "[[ruleset.r.bypass_actors]]\ntype = \"team\"\nname = \"\""
            )),
            [Code::EmptyActorName]
        );
        assert_eq!(
            codes(&ruleset(
                "[ruleset.r.rules.pull_request.dismissal_restriction]\nenabled = true\n\
                allowed_actors = [{ type = \"team\", name = \" \" }]"
            )),
            [Code::EmptyActorName]
        );
    }

    #[test]
    fn rejects_meta_that_is_not_one_segment() {
        let config: Config = toml::from_str("[meta]\nowner = \"takumin\"\nrepo = \"a/b\"").unwrap();
        let errors = check(&config, false);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, Code::InvalidMeta);
        assert_eq!(errors[0].path.as_deref(), Some("meta.repo"));
        let config: Config =
            toml::from_str("[meta]\nowner = \"takumin\"\nrepo = \"ghrs\"").unwrap();
        assert!(check(&config, false).is_empty());
    }

    #[test]
    fn unmanaged_entries_must_be_key_paths() {
        let errors = errors(
            "[meta]\nunmanaged = [\"actions\", \"ruleset.main.rules.pull_request\", \
            \"repository.has_wikis\"]",
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, Code::UnknownUnmanaged);
        assert_eq!(errors[0].path.as_deref(), Some("meta.unmanaged[2]"));
        assert!(errors[0].help.as_deref().unwrap().contains("has_wiki"));
    }

    #[test]
    fn checks_actions() {
        let codes = |toml: &str| errors(toml).iter().map(|d| d.code).collect::<Vec<_>>();
        assert!(
            codes(
                "[actions]\nallowed_actions = \"selected\"\n\
                default_workflow_permissions = \"read\"\n\
                [actions.selected_actions]\npatterns_allowed = [\"aquaproj/*\"]"
            )
            .is_empty()
        );
        assert_eq!(
            codes("[actions]\nallowed_actions = \"any\"\ndefault_workflow_permissions = \"admin\""),
            [
                Code::AllowedActionsNotAllowed,
                Code::WorkflowPermissionsNotAllowed
            ]
        );
        // Reported once, by the unknown value
        assert_eq!(
            codes("[actions]\nallowed_actions = \"any\"\n[actions.selected_actions]"),
            [Code::AllowedActionsNotAllowed]
        );

        let d = errors("[actions.selected_actions]\nverified_allowed = true");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, Code::SelectedActionsNeedSelected);
        assert_eq!(
            d[0].fix,
            Some(Fix {
                applicability: Applicability::MaybeIncorrect,
                edit: Edit::Set {
                    path: "actions.allowed_actions".to_string(),
                    value: json!("selected"),
                },
            })
        );
        let d = errors("[actions]\nallowed_actions = \"all\"\n[actions.selected_actions]");
        assert_eq!(d[0].code, Code::SelectedActionsNeedSelected);
        assert_eq!(d[0].fix, None);

        assert_eq!(
            codes("[actions]\nfork_pr_contributor_approval = \"nobody\""),
            [Code::ForkPrApprovalNotAllowed]
        );
    }

    #[test]
    fn include_claim_keys_need_a_custom_template() {
        let oidc = |body: &str| errors(&format!("[actions.oidc_subject_claim]\n{body}"));
        assert!(oidc("use_default = false\ninclude_claim_keys = [\"repo\"]").is_empty());

        let d = oidc("include_claim_keys = [\"repo\"]");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, Code::ClaimKeysNeedCustomTemplate);
        assert_eq!(
            d[0].fix,
            Some(Fix {
                applicability: Applicability::MaybeIncorrect,
                edit: Edit::Set {
                    path: "actions.oidc_subject_claim.use_default".to_string(),
                    value: json!(false),
                },
            })
        );
        let d = oidc("use_default = true\ninclude_claim_keys = [\"repo\"]");
        assert_eq!(d[0].code, Code::ClaimKeysNeedCustomTemplate);
        assert_eq!(d[0].fix, None);
    }

    #[test]
    fn fork_pr_approval_needs_a_public_repository() {
        let codes = |toml: &str, visibility: &str, scope: &Scope| {
            let config: Config = toml::from_str(toml).unwrap();
            check_context(&config, Some("takumin"), false, visibility, scope)
                .iter()
                .map(|d| d.code)
                .collect::<Vec<_>>()
        };
        let approval = "[actions]\nfork_pr_contributor_approval = \"first_time_contributors\"";
        assert!(codes(approval, "public", &ALL).is_empty());
        assert_eq!(
            codes(approval, "private", &ALL),
            [Code::ForkPrApprovalOnPrivate]
        );
        assert!(codes(approval, "private", &ACCESS).is_empty());

        // The declared visibility wins
        let made_public = format!("[repository]\nvisibility = \"public\"\n{approval}");
        assert!(codes(&made_public, "private", &ALL).is_empty());
        let made_private = format!("[repository]\nvisibility = \"private\"\n{approval}");
        assert_eq!(
            codes(&made_private, "public", &ALL),
            [Code::ForkPrApprovalOnPrivate]
        );
    }

    #[test]
    fn checks_collaborators_and_teams() {
        let toml = r#"
[collaborator.Alice]
permission = "push"

[collaborator.alice]
permission = "write"

[team.maintainers]
permission = "maintain"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let errors = check(&config, false);
        assert_eq!(errors.len(), 2, "{errors:?}");

        let toml = r#"
[collaborator.TAKUMIN]
permission = "push"

[collaborator.bob]
permission = "admin"

[team.maintainers]
permission = "maintain"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        // Owner declaration, non-push on a personal repository, team on a personal repository
        assert_eq!(
            check_context(&config, Some("takumin"), false, "public", &ACCESS).len(),
            3
        );
        // For an Organization, only the owner declaration remains
        assert_eq!(
            check_context(&config, Some("takumin"), true, "public", &ACCESS).len(),
            1
        );
    }

    #[test]
    fn rejects_org_only_rules_on_personal_repository() {
        let toml = ruleset("[ruleset.r.rules]\nmerge_queue = true\nbranch_name_pattern = false");
        let config: Config = toml::from_str(&toml).unwrap();
        assert_eq!(
            check_context(&config, Some("takumin"), false, "public", &ALL).len(),
            1
        );
        assert!(check_context(&config, Some("takumin"), true, "public", &ALL).is_empty());
    }

    /// The key is told apart from the path even when it is quoted and holds a `.`
    #[test]
    fn names_quoted_unknown_keys() {
        let d = parse_error("[repository]\n\"has.wiki\" = true");
        assert_eq!(d.code, Code::UnknownKey);
        assert_eq!(d.message, "unknown key `has.wiki`");

        // A ruleset named "rules" is not a rules table
        let d = parse_error(&format!(
            "{}\nmax_file_size = 1",
            ruleset("").replace("[ruleset.r]", "[ruleset.rules]")
        ));
        assert_eq!(d.code, Code::UnknownKey, "{d:?}");
    }

    #[test]
    fn rejects_push_rules() {
        let d = parse_error(&ruleset(
            "[ruleset.r.rules.max_file_size]\nmax_file_size = 10",
        ));
        assert_eq!(d.code, Code::PushRule);
        assert_eq!(
            d.fix.map(|f| f.edit),
            Some(Edit::Remove {
                path: "ruleset.r.rules.max_file_size".to_string()
            })
        );
    }

    #[test]
    fn rejects_roles_without_write() {
        for role in ["read", "triage"] {
            let toml = ruleset(&format!(
                "[[ruleset.r.bypass_actors]]\ntype = \"repository_role\"\nname = \"{role}\""
            ));
            assert_eq!(errors(&toml).len(), 1, "{role}");
        }
    }

    #[test]
    fn rejects_branch_only_rules_on_tag_rulesets() {
        let body = "[ruleset.r.rules]\ncopilot_code_review = true\ncode_scanning = false\n\
                    [ruleset.r.rules.pull_request]\nrequired_approving_review_count = 1";
        assert!(errors(&ruleset(body)).is_empty());
        let tag = ruleset(body).replace("\"branch\"", "\"tag\"");
        assert_eq!(errors(&tag).len(), 2);

        let pattern =
            "[ruleset.r.rules.tag_name_pattern]\noperator = \"starts_with\"\npattern = \"v\"";
        assert_eq!(errors(&ruleset(pattern)).len(), 1);
        assert!(errors(&ruleset(pattern).replace("\"branch\"", "\"tag\"")).is_empty());
    }

    #[test]
    fn ref_patterns_need_target_prefix() {
        let conditions = |include: &str| {
            format!("[ruleset.r.conditions]\ninclude = [{include}]\nexclude = [\"main\"]")
        };
        let branch = ruleset(&conditions(r#""~DEFAULT_BRANCH", "~ALL", "refs/heads/x""#));
        assert_eq!(errors(&branch).len(), 1);
        assert_eq!(errors(&ruleset(&conditions(r#""refs/tags/v*""#))).len(), 2);

        // ~DEFAULT_BRANCH is accepted for tags, but its meaning is not obvious, so reject it
        let tag = ruleset(&conditions(r#""~DEFAULT_BRANCH", "~ALL", "refs/tags/v*""#))
            .replace("\"branch\"", "\"tag\"");
        assert_eq!(errors(&tag).len(), 2);
    }

    #[test]
    fn checks_review_actors() {
        let pr = |body: &str| ruleset(&format!("[ruleset.r.rules.pull_request]\n{body}"));
        let ok = pr(
            "required_reviewers = [{ file_patterns = [\"*\"], minimum_approvals = 1, \
                reviewer = { type = \"team\", name = \"t\" } }]\n\
                    dismissal_restriction = { allowed_actors = [{ type = \"user\", name = \"u\" }, \
                        { type = \"repository_role\", name = \"admin\" }] }",
        );
        assert!(errors(&ok).is_empty());

        // The numeric ID form that import used to output
        let raw = pr(
            "required_reviewers = [{ file_patterns = [\"*\"], minimum_approvals = 1, \
                reviewer = { id = 1, type = \"Team\" } }]",
        );
        let d = parse_error(&raw);
        assert_eq!(d.code, Code::UnknownKey);
        assert_eq!(
            d.path.as_deref(),
            Some("ruleset.r.rules.pull_request.required_reviewers[0].reviewer.id")
        );
        let bad = pr(
            "required_reviewers = [{ file_patterns = [\"*\"], minimum_approvals = 1, \
                reviewer = { type = \"user\", name = \"u\" } }]\n\
                    dismissal_restriction = { allowed_actors = [{ type = \"team\", name = \"1\" }, \
                        { type = \"repository_role\", name = \"write\" }] }",
        );
        // A team slug can be all digits, so team "1" is not an error
        assert_eq!(errors(&bad).len(), 2);

        let config: Config = toml::from_str(&ok).unwrap();
        assert_eq!(
            check_context(&config, Some("takumin"), false, "public", &ALL).len(),
            2
        );
        assert!(check_context(&config, Some("takumin"), true, "public", &ALL).is_empty());

        // A team in dismissal_restriction gets only the error that removes the whole table
        let team =
            pr("dismissal_restriction = { allowed_actors = [{ type = \"team\", name = \"t\" }] }");
        let config: Config = toml::from_str(&team).unwrap();
        let codes: Vec<Code> = check_context(&config, Some("takumin"), false, "public", &ALL)
            .iter()
            .map(|d| d.code)
            .collect();
        assert_eq!(codes, [Code::DismissalRestrictionOnPersonal]);
    }

    #[test]
    fn apps_are_allowed_only_as_dismissal_actors() {
        let pr = |body: &str| ruleset(&format!("[ruleset.r.rules.pull_request]\n{body}"));
        let dismissal = |name: &str| {
            pr(&format!(
                "dismissal_restriction = {{ allowed_actors = [\
                    {{ type = \"integration\", name = \"{name}\" }}] }}"
            ))
        };
        assert!(errors(&dismissal("takumin-pr-approver")).is_empty());

        let errors_of = |toml: &str| errors(toml).into_iter().map(|d| d.code).collect::<Vec<_>>();
        assert_eq!(errors_of(&dismissal("self")), [Code::SelfNeedsApp]);
        let config: Config = toml::from_str(&dismissal("self")).unwrap();
        assert!(check(&config, true).is_empty());

        // required_reviewers rejects Apps with 422 (observed)
        let reviewer = pr(
            "required_reviewers = [{ file_patterns = [\"*\"], minimum_approvals = 1, \
            reviewer = { type = \"integration\", name = \"takumin-pr-approver\" } }]",
        );
        assert_eq!(errors_of(&reviewer), [Code::ReviewActorTypeNotAllowed]);
    }

    /// Declarations outside `--only` are neither applied nor checked against the repository
    #[test]
    fn checks_only_what_is_in_scope() {
        let toml = format!(
            "[repository]\nvisibility = \"public\"\n\n\
                [collaborator.bob]\npermission = \"admin\"\n\n\
                [team.maintainers]\npermission = \"maintain\"\n\n{}",
            ruleset("[[ruleset.r.bypass_actors]]\ntype = \"organization_admin\"")
        );
        let config: Config = toml::from_str(&toml).unwrap();
        assert!(check_context(&config, Some("takumin"), false, "public", &REPOSITORY).is_empty());

        // The declared visibility is not applied, so the current one decides
        let rulesets = Scope {
            repository: false,
            actions: false,
            rulesets: true,
            collaborators: false,
            teams: false,
        };
        let codes: Vec<Code> = check_context(&config, Some("takumin"), false, "private", &rulesets)
            .iter()
            .map(|d| d.code)
            .collect();
        assert_eq!(
            codes,
            [Code::OrganizationAdminOnPersonal, Code::RulesetsOnPrivate]
        );
    }

    #[test]
    fn rejects_rulesets_on_private_repository() {
        let config: Config = toml::from_str(&ruleset("")).unwrap();
        assert_eq!(
            check_context(&config, Some("takumin"), false, "private", &ALL).len(),
            1
        );
        assert!(check_context(&config, Some("takumin"), false, "private", &REPOSITORY).is_empty());

        let public: Config = toml::from_str(&format!(
            "[repository]\nvisibility = \"public\"\n{}",
            ruleset("")
        ))
        .unwrap();
        assert!(check_context(&public, Some("takumin"), false, "private", &ALL).is_empty());
    }

    #[test]
    fn organization_admin_has_a_machine_applicable_fix() {
        let toml = ruleset(
            "[[ruleset.r.bypass_actors]]\ntype = \"deploy_key\"\n\n\
            [[ruleset.r.bypass_actors]]\ntype = \"organization_admin\"",
        );
        let config: Config = toml::from_str(&toml).unwrap();
        let errors = check_context(&config, Some("takumin"), false, "public", &ALL);
        assert_eq!(errors.len(), 1);
        let d = &errors[0];
        assert_eq!(d.code, Code::OrganizationAdminOnPersonal);
        assert_eq!(d.path.as_deref(), Some("ruleset.r.bypass_actors[1]"));
        assert_eq!(
            d.fix,
            Some(Fix {
                applicability: Applicability::MachineApplicable,
                edit: Edit::Remove {
                    path: "ruleset.r.bypass_actors[1]".to_string()
                },
            })
        );
    }

    #[test]
    fn ref_pattern_fix_adds_the_prefix_only_to_bare_names() {
        let toml = ruleset("[ruleset.r.conditions]\ninclude = [\"main\", \"refs/tags/v*\"]");
        let errors = errors(&toml);
        assert_eq!(errors.len(), 2);
        assert_eq!(
            errors[0].fix.as_ref().map(|f| &f.edit),
            Some(&Edit::Set {
                path: "ruleset.r.conditions.include[0]".to_string(),
                value: serde_json::json!("refs/heads/main"),
            })
        );
        assert!(errors[1].fix.is_none());
    }
}
