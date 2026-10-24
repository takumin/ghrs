//! Diff model. Compare the declaration with the current state and build
//! per-resource changes.
//!
//! Values in the diff are shaped as written in `.ghrs.toml` (names, TOML-side key names).
//! The comparison itself is already done in API vocabulary, so conversion here is for
//! display only.

use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use crate::model::Repository;
use crate::resolve::Directory;
use crate::schema::{for_each_review_actor, review_actor_type_to_toml};
use crate::state::{Actor, RulesetState};

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    NoChange,
    Create,
    Update,
    Delete,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Same,
    Add,
    Remove,
    Change,
}

#[derive(Serialize, Debug)]
pub struct FieldChange {
    pub op: Op,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Json>,
}

#[derive(Serialize, Debug)]
pub struct ResourceChange {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub action: Action,
    pub fields: Vec<FieldChange>,
    /// Changes that weaken protection
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub destructive: Vec<String>,
}

/// Compare only the declared fields.
pub fn repository(decl: &Repository, current: &Json) -> ResourceChange {
    let mut fields = Fields::default();
    let declared = serde_json::to_value(decl).unwrap_or_default();

    for (key, want) in declared.as_object().into_iter().flatten() {
        let have = match (&current[key], want) {
            // description and homepage return null when unset
            (Json::Null, Json::String(_)) => json!(""),
            (have, _) => have.clone(),
        };
        match (&have, want) {
            (Json::Array(have), Json::Array(want)) => fields.set(key, have, want),
            _ => fields.value(key.clone(), Some(&have), Some(want)),
        }
    }

    let mut destructive = Vec::new();
    if current["visibility"] == "public" && decl.visibility.as_deref() == Some("private") {
        destructive.push(
            "changes visibility from public to private; rulesets stop working on the free plan"
                .to_string(),
        );
    }

    ResourceChange {
        kind: "repository",
        name: None,
        action: if fields.changed() {
            Action::Update
        } else {
            Action::NoChange
        },
        fields: fields.0,
        destructive,
    }
}

/// Diff of the whole set of rulesets. Ordered as declared, with deletions after them in
/// current-state order.
///
/// The second return value is the names of rulesets that are not declared but are kept
/// because `--prune` is absent.
pub fn rulesets(
    desired: &[RulesetState],
    current: &[RulesetState],
    prune: bool,
    dir: &Directory,
) -> (Vec<ResourceChange>, Vec<String>) {
    let mut changes = Vec::new();
    for want in desired {
        let have = current.iter().find(|c| c.name == want.name);
        changes.push(ruleset(Some(want), have, dir));
    }

    let mut ignored = Vec::new();
    for have in current {
        if desired.iter().any(|d| d.name == have.name) {
            continue;
        }
        if prune {
            changes.push(ruleset(None, Some(have), dir));
        } else {
            ignored.push(have.name.clone());
        }
    }
    (changes, ignored)
}

fn ruleset(
    want: Option<&RulesetState>,
    have: Option<&RulesetState>,
    dir: &Directory,
) -> ResourceChange {
    let name = want.or(have).map(|s| s.name.clone());

    let Some(want) = want else {
        return ResourceChange {
            kind: "ruleset",
            destructive: vec![format!(
                "deletes ruleset \"{}\"",
                name.as_deref().unwrap_or_default()
            )],
            name,
            action: Action::Delete,
            fields: Vec::new(),
        };
    };

    let mut fields = Fields::default();
    fields.value(
        "target".to_string(),
        have.map(|h| json!(h.target)).as_ref(),
        Some(&json!(want.target)),
    );
    fields.value(
        "enforcement".to_string(),
        have.map(|h| json!(h.enforcement)).as_ref(),
        Some(&json!(want.enforcement)),
    );
    fields.set(
        "conditions.include",
        &have.map(|h| strings(&h.include)).unwrap_or_default(),
        &strings(&want.include),
    );
    fields.set(
        "conditions.exclude",
        &have.map(|h| strings(&h.exclude)).unwrap_or_default(),
        &strings(&want.exclude),
    );

    let mut destructive = Vec::new();
    let no_actors = Vec::new();
    let have_actors = have.map_or(&no_actors, |h| &h.actors);
    for actor in &want.actors {
        let same_actor =
            |a: &&Actor| a.actor_type == actor.actor_type && a.actor_id == actor.actor_id;
        match have_actors.iter().find(same_actor) {
            None => {
                fields.push(Op::Add, "bypass_actors", None, Some(actor_json(actor, dir)));
                if have.is_some() {
                    destructive.push(format!(
                        "ruleset \"{}\": adds {} to bypass_actors",
                        want.name,
                        actor_label(actor, dir)
                    ));
                }
            }
            Some(old) if old.mode == actor.mode => {
                fields.push(
                    Op::Same,
                    "bypass_actors",
                    None,
                    Some(actor_json(actor, dir)),
                );
            }
            Some(old) => {
                fields.push(
                    Op::Change,
                    &format!("bypass_actors[{}].mode", actor_label(actor, dir)),
                    Some(json!(old.mode)),
                    Some(json!(actor.mode)),
                );
                if bypass_strength(&actor.mode) > bypass_strength(&old.mode) {
                    destructive.push(format!(
                        "ruleset \"{}\": widens the mode of {} from {} to {}",
                        want.name,
                        actor_label(actor, dir),
                        old.mode,
                        actor.mode
                    ));
                }
            }
        }
    }
    for actor in have_actors {
        let kept = want
            .actors
            .iter()
            .any(|a| a.actor_type == actor.actor_type && a.actor_id == actor.actor_id);
        if !kept {
            fields.push(
                Op::Remove,
                "bypass_actors",
                Some(actor_json(actor, dir)),
                None,
            );
        }
    }

    let mut rule_names: Vec<&String> = want.rules.keys().collect();
    if let Some(have) = have {
        rule_names.extend(have.rules.keys().filter(|k| !want.rules.contains_key(*k)));
    }
    rule_names.sort();
    for rule in rule_names {
        let path = format!("rules.{rule}");
        let before = have.and_then(|h| h.rules.get(rule));
        let after = want.rules.get(rule);
        match (before, after) {
            (None, None) => {}
            (Some(None), Some(None)) => fields.push(Op::Same, &path, None, Some(json!(true))),
            (None, Some(None)) => fields.push(Op::Add, &path, None, Some(json!(true))),
            (Some(before), None) => {
                fields.push(Op::Remove, &path, Some(rule_json(rule, before, dir)), None)
            }
            (None, Some(Some(params))) if params.as_object().is_some_and(Map::is_empty) => {
                fields.push(Op::Add, &path, None, Some(json!(true)));
            }
            (before, Some(Some(after))) => {
                let before = before.and_then(|b| b.as_ref()).map(|b| named(rule, b, dir));
                let after = named(rule, after, dir);
                fields.params(&path, rule, before.as_ref(), Some(&after), dir);
            }
            (Some(Some(before)), Some(None)) => fields.push(
                Op::Change,
                &path,
                Some(rule_json(rule, &Some(before.clone()), dir)),
                Some(json!(true)),
            ),
        }
    }

    if let Some(have) = have
        && have.enforcement == "active"
        && want.enforcement == "disabled"
    {
        destructive.push(format!(
            "ruleset \"{}\": changes enforcement from active to disabled",
            want.name
        ));
    }
    if let Some(have) = have {
        destructive.extend(
            fields
                .0
                .iter()
                // A rule the ruleset does not have yet only adds protection, whatever its
                // parameters
                .filter(|f| rule_of(&f.path).is_none_or(|rule| have.rules.contains_key(rule)))
                .filter_map(weakening)
                .map(|message| format!("ruleset \"{}\": {message}", want.name)),
        );
    }

    let action = match have {
        None => Action::Create,
        Some(_) if fields.changed() => Action::Update,
        Some(_) => Action::NoChange,
    };
    ResourceChange {
        kind: "ruleset",
        name,
        action,
        fields: fields.0,
        destructive,
    }
}

#[derive(Default)]
struct Fields(Vec<FieldChange>);

impl Fields {
    fn changed(&self) -> bool {
        self.0.iter().any(|f| f.op != Op::Same)
    }

    fn push(&mut self, op: Op, path: &str, before: Option<Json>, after: Option<Json>) {
        self.0.push(FieldChange {
            op,
            path: path.to_string(),
            before,
            after,
        });
    }

    fn value(&mut self, path: String, before: Option<&Json>, after: Option<&Json>) {
        let (op, before, after) = match (before, after) {
            (Some(b), Some(a)) if b == a => (Op::Same, None, Some(a.clone())),
            (Some(b), Some(a)) => (Op::Change, Some(b.clone()), Some(a.clone())),
            (None, Some(a)) => (Op::Add, None, Some(a.clone())),
            (Some(b), None) => (Op::Remove, Some(b.clone()), None),
            (None, None) => return,
        };
        self.0.push(FieldChange {
            op,
            path,
            before,
            after,
        });
    }

    /// Compare arrays as order-independent sets and produce per-element diffs.
    fn set(&mut self, path: &str, before: &[Json], after: &[Json]) {
        for item in after {
            let op = if before.contains(item) {
                Op::Same
            } else {
                Op::Add
            };
            self.push(op, path, None, Some(item.clone()));
        }
        for item in before {
            if !after.contains(item) {
                self.push(Op::Remove, path, Some(item.clone()), None);
            }
        }
    }

    /// Recursively compare rule parameters.
    fn params(
        &mut self,
        path: &str,
        rule: &str,
        before: Option<&Json>,
        after: Option<&Json>,
        dir: &Directory,
    ) {
        match (before, after) {
            (Some(Json::Object(_)) | None, Some(Json::Object(_)))
            | (Some(Json::Object(_)), None) => {
                let empty = Map::new();
                let b = before.and_then(Json::as_object).unwrap_or(&empty);
                let a = after.and_then(Json::as_object).unwrap_or(&empty);
                let mut keys: Vec<&String> = a
                    .keys()
                    .chain(b.keys().filter(|k| !a.contains_key(*k)))
                    .collect();
                keys.sort();
                for key in keys {
                    let child = format!("{path}.{}", display_key(rule, key));
                    if rule == "required_status_checks" && key == "required_status_checks" {
                        let b = checks_json(b.get(key), dir);
                        let a = checks_json(a.get(key), dir);
                        self.set(&child, &b, &a);
                    } else {
                        self.params(&child, rule, b.get(key), a.get(key), dir);
                    }
                }
            }
            (Some(Json::Array(b)), Some(Json::Array(a))) => self.set(path, b, a),
            (None, Some(Json::Array(a))) => self.set(path, &[], a),
            (Some(Json::Array(b)), None) => self.set(path, b, &[]),
            _ => self.value(path.to_string(), before, after),
        }
    }
}

/// Strength of a bypass. Higher means weaker protection. `pull_request` exempts only via PRs,
/// `always` also allows direct pushes, and `exempt` skips rule evaluation altogether.
fn bypass_strength(mode: &str) -> u8 {
    match mode {
        "pull_request" => 0,
        "always" => 1,
        _ => 2,
    }
}

/// Parameters whose change in one direction weakens protection, by their path in the diff.
/// Parameters without a clear direction (patterns, merge queue tuning, etc.) are not listed.
const WEAKENING_PARAMS: &[(&str, Weakens)] = &[
    (
        "rules.pull_request.required_approving_review_count",
        Weakens::Decrease,
    ),
    (
        "rules.pull_request.dismiss_stale_reviews_on_push",
        Weakens::Disable,
    ),
    (
        "rules.pull_request.require_code_owner_review",
        Weakens::Disable,
    ),
    (
        "rules.pull_request.require_extra_approval_for_unattributed_changes",
        Weakens::Disable,
    ),
    (
        "rules.pull_request.require_last_push_approval",
        Weakens::Disable,
    ),
    (
        "rules.pull_request.required_review_thread_resolution",
        Weakens::Disable,
    ),
    ("rules.pull_request.required_reviewers", Weakens::Remove),
    (
        "rules.pull_request.dismissal_restriction.enabled",
        Weakens::Disable,
    ),
    ("rules.required_status_checks.strict", Weakens::Disable),
    (
        "rules.required_status_checks.do_not_enforce_on_create",
        Weakens::Enable,
    ),
    ("rules.required_status_checks.checks", Weakens::Remove),
    (
        "rules.required_deployments.required_deployment_environments",
        Weakens::Remove,
    ),
    (
        "rules.update.update_allows_fetch_and_merge",
        Weakens::Enable,
    ),
    ("rules.workflows.do_not_enforce_on_create", Weakens::Enable),
    ("rules.workflows.workflows", Weakens::Remove),
    ("rules.code_scanning.code_scanning_tools", Weakens::Remove),
    ("rules.code_coverage.minimum_coverage", Weakens::Decrease),
    ("rules.code_coverage.max_coverage_drop", Weakens::Increase),
];

#[derive(Clone, Copy)]
enum Weakens {
    /// `true` -> `false`
    Disable,
    /// `false` -> `true`
    Enable,
    Decrease,
    Increase,
    /// Removing an element of the array
    Remove,
}

/// Why a field change on an existing ruleset weakens protection, if it does.
fn weakening(field: &FieldChange) -> Option<String> {
    let path = field.path.as_str();
    match (field.op, path) {
        (Op::Change, "target") => {
            return Some(format!(
                "changes target from {} to {}; the refs it covered lose protection",
                field.before.as_ref()?,
                field.after.as_ref()?
            ));
        }
        (Op::Remove, "conditions.include") => {
            return Some(format!(
                "removes {} from conditions.include",
                field.before.as_ref()?
            ));
        }
        (Op::Add, "conditions.exclude") => {
            return Some(format!(
                "adds {} to conditions.exclude",
                field.after.as_ref()?
            ));
        }
        (Op::Remove, _) if path.starts_with("rules.") && !path["rules.".len()..].contains('.') => {
            return Some(format!("removes rule {}", &path["rules.".len()..]));
        }
        _ => {}
    }

    let (_, weakens) = WEAKENING_PARAMS.iter().find(|(p, _)| *p == path)?;
    let (before, after) = (field.before.as_ref(), field.after.as_ref());
    let weakened = match (weakens, field.op) {
        (Weakens::Remove, Op::Remove) => return Some(format!("removes an entry from {path}")),
        (Weakens::Disable, Op::Change | Op::Remove) => before == Some(&json!(true)),
        (Weakens::Enable, Op::Change | Op::Add) => {
            after == Some(&json!(true)) && before != Some(&json!(true))
        }
        (Weakens::Decrease, Op::Change) => {
            before.and_then(Json::as_f64) > after.and_then(Json::as_f64)
        }
        (Weakens::Increase, Op::Change) => {
            before.and_then(Json::as_f64) < after.and_then(Json::as_f64)
        }
        _ => false,
    };
    weakened.then(|| {
        format!(
            "changes {path} from {} to {}",
            before.map_or_else(|| "unset".to_string(), Json::to_string),
            after.map_or_else(|| "unset".to_string(), Json::to_string),
        )
    })
}

/// The rule a diff path belongs to (`rules.<rule>...`).
fn rule_of(path: &str) -> Option<&str> {
    path.strip_prefix("rules.")?.split('.').next()
}

/// Only `required_status_checks` uses short key names on the TOML side.
fn display_key<'a>(rule: &str, key: &'a str) -> &'a str {
    match (rule, key) {
        ("required_status_checks", "strict_required_status_checks_policy") => "strict",
        ("required_status_checks", "required_status_checks") => "checks",
        _ => key,
    }
}

fn checks_json(v: Option<&Json>, dir: &Directory) -> Vec<Json> {
    v.and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let mut entry = Map::new();
            entry.insert("context".to_string(), c["context"].clone());
            if let Some(id) = c["integration_id"].as_u64() {
                entry.insert("integration".to_string(), json!(dir.app_name(id)));
            }
            Json::Object(entry)
        })
        .collect()
}

/// Display a `pull_request` actor (`{id, type}`) in the same `{type, name}` form as the
/// declaration. The comparison is done by ID; names are restored only for display.
fn named(rule: &str, params: &Json, dir: &Directory) -> Json {
    let mut params = params.clone();
    if rule == "pull_request" {
        let _ = for_each_review_actor(&mut params, |actor| {
            let api = actor["type"].as_str().unwrap_or_default().to_string();
            let kind = review_actor_type_to_toml(&api).unwrap_or(&api).to_string();
            let name = dir
                .actor_name(&api, actor["id"].as_u64())
                .unwrap_or_default();
            *actor = json!({ "type": kind, "name": name });
            Ok::<(), ()>(())
        });
    }
    params
}

/// Value for showing a deleted rule on a single line.
fn rule_json(rule: &str, params: &Option<Json>, dir: &Directory) -> Json {
    let Some(params) = params else {
        return json!(true);
    };
    let params = &named(rule, params, dir);
    let Some(map) = params.as_object() else {
        return params.clone();
    };
    Json::Object(
        map.iter()
            .map(|(k, v)| {
                let v = if rule == "required_status_checks" && k == "required_status_checks" {
                    Json::Array(checks_json(Some(v), dir))
                } else {
                    v.clone()
                };
                (display_key(rule, k).to_string(), v)
            })
            .collect(),
    )
}

fn actor_json(actor: &Actor, dir: &Directory) -> Json {
    let mut entry = Map::new();
    let kind = crate::schema::actor_type_to_toml(&actor.actor_type).unwrap_or(&actor.actor_type);
    entry.insert("type".to_string(), json!(kind));
    if let Some(name) = dir.actor_name(&actor.actor_type, actor.actor_id) {
        entry.insert("name".to_string(), json!(name));
    }
    entry.insert("mode".to_string(), json!(actor.mode));
    Json::Object(entry)
}

pub fn actor_label(actor: &Actor, dir: &Directory) -> String {
    let kind = crate::schema::actor_type_to_toml(&actor.actor_type).unwrap_or(&actor.actor_type);
    match dir.actor_name(&actor.actor_type, actor.actor_id) {
        Some(name) => format!("{kind} \"{name}\""),
        None => kind.to_string(),
    }
}

fn strings(v: &[String]) -> Vec<Json> {
    v.iter().map(|s| json!(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(enforcement: &str, actors: Vec<Actor>, rules: Json) -> RulesetState {
        RulesetState::from_api(&json!({
            "id": 1,
            "name": "r",
            "target": "branch",
            "enforcement": enforcement,
            "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
            "bypass_actors": actors.iter().map(|a| json!({
                "actor_id": a.actor_id, "actor_type": a.actor_type, "bypass_mode": a.mode,
            })).collect::<Vec<_>>(),
            "rules": rules,
        }))
        .unwrap()
    }

    fn admin(mode: &str) -> Actor {
        Actor {
            actor_type: "RepositoryRole".to_string(),
            actor_id: Some(5),
            mode: mode.to_string(),
        }
    }

    fn changed(change: &ResourceChange) -> Vec<(Op, &str)> {
        change
            .fields
            .iter()
            .filter(|f| f.op != Op::Same)
            .map(|f| (f.op, f.path.as_str()))
            .collect()
    }

    #[test]
    fn identical_states_have_no_changes() {
        let rules =
            json!([{ "type": "deletion" }, { "type": "pull_request", "parameters": { "x": 1 } }]);
        let have = state("active", vec![admin("always")], rules.clone());
        let want = state("active", vec![admin("always")], rules);
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(changes[0].action, Action::NoChange);
    }

    #[test]
    fn descends_to_fields() {
        let have = state(
            "disabled",
            vec![],
            json!([
                { "type": "pull_request", "parameters": { "required_approving_review_count": 0 } },
                { "type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": true,
                    "required_status_checks": [{ "context": "test" }, { "context": "coverage" }],
                } },
            ]),
        );
        let want = state(
            "active",
            vec![],
            json!([
                { "type": "pull_request", "parameters": { "required_approving_review_count": 1 } },
                { "type": "required_signatures" },
                { "type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": true,
                    "required_status_checks": [{ "context": "test" }],
                } },
            ]),
        );
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(changes[0].action, Action::Update);
        assert_eq!(
            changed(&changes[0]),
            [
                (Op::Change, "enforcement"),
                (
                    Op::Change,
                    "rules.pull_request.required_approving_review_count"
                ),
                (Op::Add, "rules.required_signatures"),
                (Op::Remove, "rules.required_status_checks.checks"),
            ]
        );
    }

    #[test]
    fn arrays_are_compared_as_sets() {
        let have = state(
            "active",
            vec![],
            json!([{ "type": "pull_request", "parameters": { "m": ["squash", "rebase"] } }]),
        );
        let want = state(
            "active",
            vec![],
            json!([{ "type": "pull_request", "parameters": { "m": ["rebase", "squash"] } }]),
        );
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(changes[0].action, Action::NoChange);
    }

    #[test]
    fn undeclared_rulesets_need_prune() {
        let have = state("active", vec![], json!([]));
        let (changes, ignored) = rulesets(
            &[],
            std::slice::from_ref(&have),
            false,
            &Directory::default(),
        );
        assert!(changes.is_empty());
        assert_eq!(ignored, ["r"]);

        let (changes, ignored) = rulesets(&[], &[have], true, &Directory::default());
        assert_eq!(changes[0].action, Action::Delete);
        assert_eq!(changes[0].destructive.len(), 1);
        assert!(ignored.is_empty());
    }

    #[test]
    fn flags_weakening_changes() {
        let have = state("active", vec![admin("always")], json!([]));
        let want = state(
            "disabled",
            vec![
                admin("pull_request"),
                Actor {
                    actor_type: "OrganizationAdmin".to_string(),
                    actor_id: None,
                    mode: "always".to_string(),
                },
            ],
            json!([]),
        );
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(
            changed(&changes[0]),
            [
                (Op::Change, "enforcement"),
                (Op::Change, "bypass_actors[repository_role \"admin\"].mode"),
                (Op::Add, "bypass_actors"),
            ]
        );
        assert_eq!(changes[0].destructive.len(), 2);

        // pull_request -> always widens the scope of the bypass
        let have = state("active", vec![admin("pull_request")], json!([]));
        let want = state("active", vec![admin("always")], json!([]));
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(changes[0].destructive.len(), 1);
    }

    #[test]
    fn flags_weakening_rules_and_conditions() {
        let have = RulesetState::from_api(&json!({
            "id": 1, "name": "r", "target": "branch", "enforcement": "active",
            "conditions": { "ref_name": {
                "include": ["~DEFAULT_BRANCH", "refs/heads/release"],
                "exclude": [],
            } },
            "bypass_actors": [],
            "rules": [
                { "type": "deletion" },
                { "type": "pull_request", "parameters": {
                    "required_approving_review_count": 2,
                    "require_code_owner_review": true,
                    "dismiss_stale_reviews_on_push": false,
                } },
                { "type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": true,
                    "required_status_checks": [{ "context": "test" }, { "context": "build" }],
                } },
            ],
        }))
        .unwrap();
        let want = RulesetState::from_api(&json!({
            "id": 1, "name": "r", "target": "tag", "enforcement": "active",
            "conditions": { "ref_name": {
                "include": ["~DEFAULT_BRANCH"],
                "exclude": ["refs/heads/tmp"],
            } },
            "bypass_actors": [],
            "rules": [
                { "type": "pull_request", "parameters": {
                    "required_approving_review_count": 1,
                    "require_code_owner_review": false,
                    // Strengthening is not flagged
                    "dismiss_stale_reviews_on_push": true,
                } },
                { "type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": true,
                    "required_status_checks": [{ "context": "test" }],
                } },
            ],
        }))
        .unwrap();
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(
            changes[0].destructive,
            [
                "ruleset \"r\": changes target from \"branch\" to \"tag\"; the refs it covered \
                    lose protection",
                "ruleset \"r\": removes \"refs/heads/release\" from conditions.include",
                "ruleset \"r\": adds \"refs/heads/tmp\" to conditions.exclude",
                "ruleset \"r\": removes rule deletion",
                "ruleset \"r\": changes rules.pull_request.require_code_owner_review from true \
                    to false",
                "ruleset \"r\": changes rules.pull_request.required_approving_review_count from \
                    2 to 1",
                "ruleset \"r\": removes an entry from rules.required_status_checks.checks",
            ]
        );
    }

    /// A rule the ruleset does not have yet adds protection, whatever its parameters
    #[test]
    fn adding_a_rule_is_not_weakening() {
        let have = state("active", vec![], json!([]));
        let want = state(
            "active",
            vec![],
            json!([
                { "type": "update", "parameters": { "update_allows_fetch_and_merge": true } },
                { "type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": false,
                    "do_not_enforce_on_create": true,
                    "required_status_checks": [{ "context": "test" }],
                } },
            ]),
        );
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(changes[0].action, Action::Update);
        assert!(changes[0].destructive.is_empty());

        // On a rule it already has, the same parameter still weakens it
        let have = state("active", vec![], json!([{ "type": "update" }]));
        let want = state(
            "active",
            vec![],
            json!([{ "type": "update", "parameters": { "update_allows_fetch_and_merge": true } }]),
        );
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(
            changes[0].destructive,
            [
                "ruleset \"r\": changes rules.update.update_allows_fetch_and_merge from unset to true"
            ]
        );
    }

    /// A new ruleset weakens nothing, whatever it declares
    #[test]
    fn creation_is_not_weakening() {
        let want = state("active", vec![], json!([]));
        let (changes, _) = rulesets(&[want], &[], false, &Directory::default());
        assert_eq!(changes[0].action, Action::Create);
        assert!(changes[0].destructive.is_empty());
    }

    #[test]
    fn private_apps_compare_by_id() {
        let app = |id| Actor {
            actor_type: "Integration".to_string(),
            actor_id: Some(id),
            mode: "always".to_string(),
        };
        let have = state("active", vec![app(999)], json!([]));
        let want = state("active", vec![app(999)], json!([]));
        let (changes, _) = rulesets(&[want], &[have], false, &Directory::default());
        assert_eq!(changes[0].action, Action::NoChange);
        let actor = changes[0]
            .fields
            .iter()
            .find(|f| f.path == "bypass_actors")
            .unwrap();
        assert_eq!(
            actor.after,
            Some(json!({ "type": "integration", "name": "999", "mode": "always" }))
        );
    }

    /// Compare by ID; restore names only for display
    #[test]
    fn review_actors_are_shown_by_name() {
        let rules = |team: u64| {
            json!([{ "type": "pull_request", "parameters": {
                "required_reviewers": [{ "file_patterns": ["*"], "minimum_approvals": 1,
                                        "reviewer": { "id": team, "type": "Team" } }],
            } }])
        };
        let dir = Directory::default().with_team("old", 1).with_team("new", 2);
        let have = state("active", vec![], rules(1));
        let want = state("active", vec![], rules(2));
        let (changes, _) = rulesets(&[want], &[have], false, &dir);
        let fields: Vec<_> = changes[0]
            .fields
            .iter()
            .filter(|f| f.op != Op::Same)
            .collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(
            fields[0].after.as_ref().unwrap()["reviewer"],
            json!({ "type": "team", "name": "new" })
        );
        assert_eq!(
            fields[1].before.as_ref().unwrap()["reviewer"],
            json!({ "type": "team", "name": "old" })
        );
    }

    #[test]
    fn apps_in_pull_request_are_shown_by_slug() {
        let rules = |id: u64| {
            json!([{ "type": "pull_request", "parameters": {
                "dismissal_restriction": { "enabled": true, "allowed_actors": [
                    { "id": id, "type": "IntegrationInstallation" },
                ] },
            } }])
        };
        let dir = Directory::default().with_installation("approver", 152340499);
        let have = state("active", vec![], rules(100451791));
        let want = state("active", vec![], rules(152340499));
        let (changes, _) = rulesets(&[want], &[have], false, &dir);
        let changed: Vec<_> = changes[0]
            .fields
            .iter()
            .filter(|f| f.op != Op::Same)
            .map(|f| {
                (
                    f.op,
                    f.path.as_str(),
                    f.before.as_ref().or(f.after.as_ref()),
                )
            })
            .collect();
        let path = "rules.pull_request.dismissal_restriction.allowed_actors";
        assert_eq!(
            changed,
            [
                (
                    Op::Add,
                    path,
                    Some(&json!({ "type": "integration", "name": "approver" }))
                ),
                // An installation that cannot be listed stays numeric
                (
                    Op::Remove,
                    path,
                    Some(&json!({ "type": "integration", "name": "100451791" }))
                ),
            ]
        );
    }

    #[test]
    fn repository_compares_declared_fields_only() {
        let decl: Repository =
            toml::from_str("description = \"x\"\nhas_wiki = false\ntopics = [\"b\", \"a\"]")
                .unwrap();
        let current = json!({
            "description": null, "has_wiki": false, "has_issues": true,
            "topics": ["a", "c"], "visibility": "public",
        });
        let change = repository(&decl, &current);
        assert_eq!(change.action, Action::Update);
        assert_eq!(
            changed(&change),
            [
                (Op::Change, "description"),
                (Op::Add, "topics"),
                (Op::Remove, "topics")
            ]
        );
    }

    /// import outputs an unset homepage as an empty string
    #[test]
    fn empty_strings_match_null() {
        let decl: Repository = toml::from_str("description = \"\"\nhomepage = \"\"").unwrap();
        let change = repository(&decl, &json!({ "description": null, "homepage": null }));
        assert_eq!(change.action, Action::NoChange);
    }

    #[test]
    fn flags_visibility_downgrade() {
        let decl: Repository = toml::from_str("visibility = \"private\"").unwrap();
        let change = repository(&decl, &json!({ "visibility": "public" }));
        assert_eq!(change.destructive.len(), 1);
    }
}
