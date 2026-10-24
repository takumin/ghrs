//! GitHub Actions settings. GitHub splits them over three endpoints; ghrs reads them into one
//! object shaped like `[actions]` and writes each endpoint whose fields changed.

use anyhow::{Result, bail};
use serde_json::{Map, Value as Json};

use crate::diff::{Op, ResourceChange};
use crate::github::GitHub;
use crate::model::{Actions, SelectedActions};
use crate::plan::Step;
use crate::repo::RepoRef;

/// Fields of `PUT /repos/{o}/{r}/actions/permissions`. `enabled` is required there.
const PERMISSIONS: &[&str] = &["enabled", "allowed_actions", "sha_pinning_required"];

/// Fields of `PUT /repos/{o}/{r}/actions/permissions/workflow`.
const WORKFLOW: &[&str] = &[
    "default_workflow_permissions",
    "can_approve_pull_request_reviews",
];

/// Keys of `GET /repos/{o}/{r}/actions/permissions` that are not settings.
const READ_ONLY: &[&str] = &["selected_actions_url"];

/// The current settings in the declaration's shape, with every key GitHub returned (unknown ones
/// included, for import to warn about).
///
/// `selected_actions` is read only with `with_selected` and while `allowed_actions` is
/// `selected`: otherwise GitHub returns 409 (`All actions and workflows are allowed on this
/// repository`) (observed).
pub async fn current(gh: &mut GitHub, target: &RepoRef, with_selected: bool) -> Result<Json> {
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
    Ok(Json::Object(out))
}

fn object(v: Json) -> Result<Map<String, Json>> {
    match v {
        Json::Object(map) => Ok(map),
        _ => bail!("response of the Actions permissions API is not an object"),
    }
}

/// The writes for the changed fields. Each endpoint is sent its fields as declared, or else as
/// they are now, so it does not matter whether GitHub treats a missing field as unchanged.
pub fn step(decl: &Actions, current: &Json, change: &ResourceChange) -> Option<Step> {
    let declared = serde_json::to_value(decl).ok()?;
    let changed: Vec<&str> = change
        .fields
        .iter()
        .filter(|f| f.op != Op::Same)
        .map(|f| f.path.as_str())
        .collect();
    if changed.is_empty() {
        return None;
    }
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
    Some(Step::Actions {
        permissions: touches(PERMISSIONS).then(|| body(PERMISSIONS, &declared, current)),
        selected_actions: changed
            .iter()
            .any(|path| path.starts_with("selected_actions."))
            .then(|| {
                body(
                    SelectedActions::KEYS,
                    &declared["selected_actions"],
                    &current["selected_actions"],
                )
            }),
        workflow: touches(WORKFLOW).then(|| body(WORKFLOW, &declared, current)),
    })
}

/// The settings as a declaration, and the keys GitHub returned that ghrs does not know.
pub fn declaration(current: &Json) -> Result<(Actions, Vec<String>)> {
    let mut known = Map::new();
    let mut unknown = Vec::new();
    for (key, value) in current.as_object().into_iter().flatten() {
        match value {
            Json::Object(table) if key == "selected_actions" => {
                let mut kept = Map::new();
                for (k, v) in table {
                    if SelectedActions::KEYS.contains(&k.as_str()) {
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
    use serde_json::json;

    fn decl(text: &str) -> Actions {
        toml::from_str(text).unwrap()
    }

    fn bodies(step: Option<Step>) -> (Option<Json>, Option<Json>, Option<Json>) {
        match step {
            Some(Step::Actions {
                permissions,
                selected_actions,
                workflow,
            }) => (permissions, selected_actions, workflow),
            Some(_) => panic!("not an actions step"),
            None => (None, None, None),
        }
    }

    fn current() -> Json {
        json!({
            "enabled": true,
            "allowed_actions": "all",
            "sha_pinning_required": false,
            "default_workflow_permissions": "read",
            "can_approve_pull_request_reviews": false,
        })
    }

    #[test]
    fn writes_only_the_endpoints_whose_fields_changed() {
        let decl = decl("sha_pinning_required = true\ndefault_workflow_permissions = \"read\"");
        let change = diff::actions(&decl, &current());
        let (permissions, selected, workflow) = bodies(step(&decl, &current(), &change));
        // `enabled` is required, so the current value goes along
        assert_eq!(
            permissions,
            Some(json!({
                "enabled": true, "allowed_actions": "all", "sha_pinning_required": true,
            }))
        );
        assert_eq!(selected, None);
        assert_eq!(workflow, None);

        let decl = self::decl("enabled = true");
        let change = diff::actions(&decl, &current());
        assert!(step(&decl, &current(), &change).is_none());
    }

    #[test]
    fn selected_actions_start_from_the_declaration_when_github_has_none() {
        let decl = decl(
            "allowed_actions = \"selected\"\n\
            [selected_actions]\npatterns_allowed = [\"aquaproj/*\"]",
        );
        let change = diff::actions(&decl, &current());
        let (permissions, selected, workflow) = bodies(step(&decl, &current(), &change));
        assert_eq!(permissions.unwrap()["allowed_actions"], "selected");
        assert_eq!(
            selected,
            Some(json!({ "patterns_allowed": ["aquaproj/*"] }))
        );
        assert_eq!(workflow, None);

        let mut cur = current();
        cur["allowed_actions"] = json!("selected");
        cur["selected_actions"] = json!({
            "github_owned_allowed": true, "verified_allowed": false, "patterns_allowed": [],
        });
        let change = diff::actions(&decl, &cur);
        let (permissions, selected, _) = bodies(step(&decl, &cur, &change));
        assert_eq!(permissions, None);
        assert_eq!(
            selected,
            Some(json!({
                "github_owned_allowed": true,
                "verified_allowed": false,
                "patterns_allowed": ["aquaproj/*"],
            }))
        );
    }

    #[test]
    fn declaration_leaves_out_unknown_keys() {
        let mut cur = current();
        cur["future"] = json!(1);
        cur["selected_actions"] = json!({ "github_owned_allowed": true, "extra": [] });
        let (decl, unknown) = declaration(&cur).unwrap();
        assert_eq!(unknown, ["future", "selected_actions.extra"]);
        assert_eq!(decl.allowed_actions.as_deref(), Some("all"));
        assert_eq!(
            decl.selected_actions.unwrap().github_owned_allowed,
            Some(true)
        );
    }
}
