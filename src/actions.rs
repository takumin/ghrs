//! GitHub Actions settings. GitHub splits them over several endpoints; ghrs reads them into one
//! object shaped like `[actions]` and writes each endpoint whose fields changed.

use anyhow::{Result, bail};
use serde_json::{Map, Value as Json, json};

use crate::diff::{Op, ResourceChange};
use crate::github::GitHub;
use crate::model::{Actions, OidcSubjectClaim, SelectedActions};
use crate::plan::Step;
use crate::repo::RepoRef;

/// Fields of `PUT /repos/{o}/{r}/actions/permissions`. `enabled` is required there.
const PERMISSIONS: &[&str] = &["enabled", "allowed_actions", "sha_pinning_required"];

/// Fields of `PUT /repos/{o}/{r}/actions/permissions/workflow`.
const WORKFLOW: &[&str] = &[
    "default_workflow_permissions",
    "can_approve_pull_request_reviews",
];

/// The key ghrs gives the `approval_policy` of
/// `/repos/{o}/{r}/actions/permissions/fork-pr-contributor-approval`, its only field.
const FORK_PR_APPROVAL: &str = "fork_pr_contributor_approval";

/// The tables inside `[actions]`, each written to an endpoint of its own, with their keys.
const NESTED: &[(&str, &[&str])] = &[
    ("selected_actions", SelectedActions::KEYS),
    ("oidc_subject_claim", OidcSubjectClaim::KEYS),
];

/// Keys of `GET /repos/{o}/{r}/actions/permissions` that are not settings.
const READ_ONLY: &[&str] = &["selected_actions_url"];

/// Keys of `GET /repos/{o}/{r}/actions/oidc/customization/sub` that are not settings.
const OIDC_READ_ONLY: &[&str] = &["sub_claim_prefix"];

/// The current settings in the declaration's shape, with every key GitHub returned (unknown ones
/// included, for import to warn about).
///
/// `selected_actions` is read only with `with_selected` and while `allowed_actions` is
/// `selected`: otherwise GitHub returns 409 (`All actions and workflows are allowed on this
/// repository`) (observed). `fork_pr_contributor_approval` is read only when `public`: otherwise
/// GitHub returns 422 (`Fork PR approval is not allowed for private repositories.`) (observed).
pub async fn current(
    gh: &mut GitHub,
    target: &RepoRef,
    with_selected: bool,
    public: bool,
) -> Result<Json> {
    let mut out = object(
        gh.get(&format!("/repos/{target}/actions/permissions"))
            .await?,
    )?;
    out.retain(|key, _| !READ_ONLY.contains(&key.as_str()));
    let workflow = gh
        .get(&format!("/repos/{target}/actions/permissions/workflow"))
        .await?;
    out.extend(object(workflow)?);
    if with_selected && out.get("allowed_actions") == Some(&Json::from("selected")) {
        let selected = gh
            .get(&format!(
                "/repos/{target}/actions/permissions/selected-actions"
            ))
            .await?;
        out.insert(
            "selected_actions".to_string(),
            Json::Object(object(selected)?),
        );
    }
    if public {
        let approval = gh
            .get(&format!(
                "/repos/{target}/actions/permissions/fork-pr-contributor-approval"
            ))
            .await?;
        if let Some(policy) = object(approval)?.remove("approval_policy") {
            out.insert(FORK_PR_APPROVAL.to_string(), policy);
        }
    }
    let mut oidc = object(
        gh.get(&format!("/repos/{target}/actions/oidc/customization/sub"))
            .await?,
    )?;
    oidc.retain(|key, _| !OIDC_READ_ONLY.contains(&key.as_str()));
    out.insert("oidc_subject_claim".to_string(), Json::Object(oidc));
    Ok(Json::Object(out))
}

fn object(v: Json) -> Result<Map<String, Json>> {
    match v {
        Json::Object(map) => Ok(map),
        _ => bail!("response of the Actions permissions API is not an object"),
    }
}

/// The writes for the changed fields, as paths under `/repos/{o}/{r}/actions` with their bodies,
/// in the order to send them: `selected_actions` needs `allowed_actions = "selected"` to be set
/// first. Each endpoint is sent its fields as declared, or else as they are now, so it does not
/// matter whether GitHub treats a missing field as unchanged.
pub fn step(decl: &Actions, current: &Json, change: &ResourceChange) -> Option<Step> {
    let declared = serde_json::to_value(decl).ok()?;
    let changed: Vec<&str> = change
        .fields
        .iter()
        .filter(|f| f.op != Op::Same)
        .map(|f| f.path.as_str())
        .collect();
    let body = |keys: &[&str], declared: &Json, current: &Json| {
        let fields: Map<String, Json> = keys
            .iter()
            .filter_map(|key| {
                let value = declared.get(*key).or_else(|| current.get(*key))?;
                Some((key.to_string(), value.clone()))
            })
            .collect();
        Json::Object(fields)
    };
    let touches = |keys: &[&str]| changed.iter().any(|path| keys.contains(path));
    let touches_table = |table: &str| {
        changed.iter().any(|path| {
            path.strip_prefix(table)
                .is_some_and(|rest| rest.starts_with('.'))
        })
    };
    let table = |name: &str, keys: &[&str]| body(keys, &declared[name], &current[name]);

    let mut writes = Vec::new();
    if touches(PERMISSIONS) {
        writes.push(("/permissions", body(PERMISSIONS, &declared, current)));
    }
    if touches_table("selected_actions") {
        writes.push((
            "/permissions/selected-actions",
            table("selected_actions", SelectedActions::KEYS),
        ));
    }
    if touches(WORKFLOW) {
        writes.push(("/permissions/workflow", body(WORKFLOW, &declared, current)));
    }
    if touches(&[FORK_PR_APPROVAL]) {
        writes.push((
            "/permissions/fork-pr-contributor-approval",
            json!({ "approval_policy": declared[FORK_PR_APPROVAL] }),
        ));
    }
    if touches_table("oidc_subject_claim") {
        writes.push((
            "/oidc/customization/sub",
            table("oidc_subject_claim", OidcSubjectClaim::KEYS),
        ));
    }
    (!writes.is_empty()).then_some(Step::Actions { writes })
}

/// The settings as a declaration, and the keys GitHub returned that ghrs does not know.
pub fn declaration(current: &Json) -> Result<(Actions, Vec<String>)> {
    let mut known = Map::new();
    let mut unknown = Vec::new();
    for (key, value) in current.as_object().into_iter().flatten() {
        let nested = NESTED.iter().find(|(name, _)| name == key);
        match (nested, value) {
            (Some((_, keys)), Json::Object(table)) => {
                let mut kept = Map::new();
                for (k, v) in table {
                    if keys.contains(&k.as_str()) {
                        kept.insert(k.clone(), v.clone());
                    } else {
                        unknown.push(format!("{key}.{k}"));
                    }
                }
                known.insert(key.clone(), Json::Object(kept));
            }
            _ if Actions::KEYS.contains(&key.as_str()) => {
                known.insert(key.clone(), value.clone());
            }
            _ => unknown.push(key.clone()),
        }
    }
    Ok((serde_json::from_value(Json::Object(known))?, unknown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;

    fn decl(text: &str) -> Actions {
        toml::from_str(text).unwrap()
    }

    fn writes(decl: &Actions, current: &Json) -> Vec<(&'static str, Json)> {
        let change = diff::actions(decl, current);
        match step(decl, current, &change) {
            Some(Step::Actions { writes }) => writes,
            Some(_) => panic!("not an actions step"),
            None => Vec::new(),
        }
    }

    fn current() -> Json {
        json!({
            "enabled": true,
            "allowed_actions": "all",
            "sha_pinning_required": false,
            "default_workflow_permissions": "read",
            "can_approve_pull_request_reviews": false,
            "fork_pr_contributor_approval": "first_time_contributors",
            "oidc_subject_claim": { "use_default": true, "use_immutable_subject": true },
        })
    }

    #[test]
    fn writes_only_the_endpoints_whose_fields_changed() {
        let decl = decl("sha_pinning_required = true\ndefault_workflow_permissions = \"read\"");
        // `enabled` is required, so the current value goes along
        assert_eq!(
            writes(&decl, &current()),
            [(
                "/permissions",
                json!({
                    "enabled": true, "allowed_actions": "all", "sha_pinning_required": true,
                })
            )]
        );

        let decl = self::decl("enabled = true");
        assert!(writes(&decl, &current()).is_empty());
    }

    #[test]
    fn selected_actions_start_from_the_declaration_when_github_has_none() {
        let decl = decl(
            "allowed_actions = \"selected\"\n\
            [selected_actions]\npatterns_allowed = [\"aquaproj/*\"]",
        );
        let w = writes(&decl, &current());
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0, "/permissions");
        assert_eq!(w[0].1["allowed_actions"], "selected");
        assert_eq!(
            w[1],
            (
                "/permissions/selected-actions",
                json!({ "patterns_allowed": ["aquaproj/*"] })
            )
        );

        let mut cur = current();
        cur["allowed_actions"] = json!("selected");
        cur["selected_actions"] = json!({
            "github_owned_allowed": true, "verified_allowed": false, "patterns_allowed": [],
        });
        assert_eq!(
            writes(&decl, &cur),
            [(
                "/permissions/selected-actions",
                json!({
                    "github_owned_allowed": true,
                    "verified_allowed": false,
                    "patterns_allowed": ["aquaproj/*"],
                })
            )]
        );
    }

    #[test]
    fn writes_fork_pr_approval_under_the_api_name() {
        let decl = decl("fork_pr_contributor_approval = \"all_external_contributors\"");
        assert_eq!(
            writes(&decl, &current()),
            [(
                "/permissions/fork-pr-contributor-approval",
                json!({ "approval_policy": "all_external_contributors" })
            )]
        );
    }

    #[test]
    fn oidc_subject_claim_keeps_the_fields_not_written() {
        let decl = decl(
            "[oidc_subject_claim]\nuse_default = false\n\
            include_claim_keys = [\"repo\", \"context\"]",
        );
        assert_eq!(
            writes(&decl, &current()),
            [(
                "/oidc/customization/sub",
                json!({
                    "use_default": false,
                    "use_immutable_subject": true,
                    "include_claim_keys": ["repo", "context"],
                })
            )]
        );

        let mut cur = current();
        cur["oidc_subject_claim"] = json!({
            "use_default": false, "use_immutable_subject": true,
            "include_claim_keys": ["repo", "context"],
        });
        assert!(writes(&decl, &cur).is_empty());
    }

    #[test]
    fn declaration_leaves_out_unknown_keys() {
        let mut cur = current();
        cur["future"] = json!(1);
        cur["selected_actions"] = json!({ "github_owned_allowed": true, "extra": [] });
        cur["oidc_subject_claim"]["extra"] = json!(true);
        let (decl, unknown) = declaration(&cur).unwrap();
        assert_eq!(
            unknown,
            [
                "future",
                "oidc_subject_claim.extra",
                "selected_actions.extra"
            ]
        );
        assert_eq!(decl.allowed_actions.as_deref(), Some("all"));
        assert_eq!(
            decl.fork_pr_contributor_approval.as_deref(),
            Some("first_time_contributors")
        );
        assert_eq!(
            decl.selected_actions.unwrap().github_owned_allowed,
            Some(true)
        );
        assert_eq!(decl.oidc_subject_claim.unwrap().use_default, Some(true));
    }
}
