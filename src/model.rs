//! Schema of `.ghrs.toml`.
//!
//! Field declaration order is the TOML output order as-is. Because TOML requires scalars to
//! come before tables, each struct is ordered scalars -> arrays -> tables.
//!
//! Unknown keys are rejected on load. If a misspelled key were silently ignored, plan would
//! report "no diff" while the setting the user meant to declare is never applied.

use serde::{Deserialize, Serialize};

/// Declares a table whose fields are all optional, and lists its keys in `KEYS`. import uses
/// `KEYS` to tell what ghrs knows from what GitHub added later.
macro_rules! optional_table {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $($(#[$field_meta:meta])* $field:ident: $ty:ty,)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Serialize, Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            $(
                $(#[$field_meta])*
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }

        impl $name {
            // Unused for tables that are not a rule's parameters
            #[allow(dead_code)]
            pub const KEYS: &[&str] = &[$(stringify!($field)),*];
        }
    };
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub repository: Repository,
    #[serde(
        rename = "ruleset",
        with = "keyed",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub rulesets: Vec<Ruleset>,
    #[serde(
        rename = "collaborator",
        with = "keyed",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub collaborators: Vec<Collaborator>,
    #[serde(
        rename = "team",
        with = "keyed",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub teams: Vec<Team>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// Only the fields that are written are compared. Unwritten fields are left untouched.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_issues: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_wiki: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_projects: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_discussions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_merge_commit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_squash_merge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_rebase_merge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_auto_merge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_branch_on_merge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ruleset {
    #[serde(skip)]
    pub name: String,
    pub target: String,
    pub enforcement: String,
    #[serde(default)]
    pub conditions: Conditions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass_actors: Vec<BypassActor>,
    #[serde(default)]
    pub rules: Rules,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Conditions {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BypassActor {
    #[serde(rename = "type")]
    pub actor_type: String,
    /// Omitted for `organization_admin` and `deploy_key`, which refer to the whole type and have
    /// no ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_bypass_mode")]
    pub mode: String,
}

fn default_bypass_mode() -> String {
    "always".to_string()
}

optional_table! {
    /// Rules of a ruleset. Rules not written here are disabled.
    ///
    /// Rules GitHub has but this table lacks cannot be declared. plan keeps them at their current
    /// values instead of disabling them, so a rule GitHub adds later is not deleted.
    pub struct Rules {
        creation: bool,
        update: Rule<Update>,
        deletion: bool,
        required_linear_history: bool,
        merge_queue: Rule<MergeQueue>,
        required_deployments: Rule<RequiredDeployments>,
        required_signatures: bool,
        pull_request: Rule<PullRequest>,
        required_status_checks: Rule<RequiredStatusChecks>,
        non_fast_forward: bool,
        commit_message_pattern: Rule<Pattern>,
        commit_author_email_pattern: Rule<Pattern>,
        committer_email_pattern: Rule<Pattern>,
        branch_name_pattern: Rule<Pattern>,
        tag_name_pattern: Rule<Pattern>,
        workflows: Rule<Workflows>,
        code_scanning: Rule<CodeScanning>,
        code_quality: Rule<CodeQuality>,
        code_coverage: Rule<CodeCoverage>,
        copilot_code_review: Rule<CopilotCodeReview>,
        license_compliance_scanning: bool,
    }
}

impl Rules {
    /// The declared rules as rule type -> `true` / `false` / table of parameters, in the TOML
    /// vocabulary.
    pub fn entries(&self) -> serde_json::Map<String, serde_json::Value> {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    }

    /// Parameter keys ghrs knows for a rule. `None` for rules without parameters and for
    /// `required_status_checks`, whose keys differ from the API's.
    pub fn param_keys(rule: &str) -> Option<&'static [&'static str]> {
        Some(match rule {
            "update" => Update::KEYS,
            "merge_queue" => MergeQueue::KEYS,
            "required_deployments" => RequiredDeployments::KEYS,
            "pull_request" => PullRequest::KEYS,
            "commit_message_pattern"
            | "commit_author_email_pattern"
            | "committer_email_pattern"
            | "branch_name_pattern"
            | "tag_name_pattern" => Pattern::KEYS,
            "workflows" => Workflows::KEYS,
            "code_scanning" => CodeScanning::KEYS,
            "code_quality" => CodeQuality::KEYS,
            "code_coverage" => CodeCoverage::KEYS,
            "copilot_code_review" => CopilotCodeReview::KEYS,
            _ => return None,
        })
    }
}

/// A rule with parameters, written as `true` / `false` or as a table of parameters.
///
/// `true` keeps the current parameters (the defaults on creation). Parameters not written in a
/// table also keep their current values.
pub enum Rule<P> {
    Enabled(bool),
    Params(P),
}

impl<P: Serialize> Serialize for Rule<P> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Rule::Enabled(enabled) => s.serialize_bool(*enabled),
            Rule::Params(params) => params.serialize(s),
        }
    }
}

/// Written by hand because `#[serde(untagged)]` would replace the error from the parameter
/// table (e.g. an unknown key) with "did not match any variant".
impl<'de, P: Deserialize<'de>> Deserialize<'de> for Rule<P> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
        use std::fmt;
        use std::marker::PhantomData;

        struct RuleVisitor<P>(PhantomData<P>);

        impl<'de, P: Deserialize<'de>> Visitor<'de> for RuleVisitor<P> {
            type Value = Rule<P>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a boolean or a table of parameters")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Rule<P>, E> {
                Ok(Rule::Enabled(v))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Rule<P>, A::Error> {
                P::deserialize(MapAccessDeserializer::new(map)).map(Rule::Params)
            }
        }

        d.deserialize_any(RuleVisitor(PhantomData))
    }
}

optional_table! {
    pub struct Update {
        update_allows_fetch_and_merge: bool,
    }
}

optional_table! {
    pub struct MergeQueue {
        check_response_timeout_minutes: u64,
        /// `ALLGREEN` or `HEADGREEN`
        grouping_strategy: String,
        max_entries_to_build: u64,
        max_entries_to_merge: u64,
        /// `MERGE`, `SQUASH` or `REBASE`
        merge_method: String,
        min_entries_to_merge: u64,
        min_entries_to_merge_wait_minutes: u64,
    }
}

optional_table! {
    pub struct RequiredDeployments {
        required_deployment_environments: Vec<String>,
    }
}

optional_table! {
    pub struct PullRequest {
        /// `merge`, `squash` and / or `rebase`
        allowed_merge_methods: Vec<String>,
        dismiss_stale_reviews_on_push: bool,
        require_code_owner_review: bool,
        /// Undocumented; returned by the API alongside the documented parameters (observed)
        require_extra_approval_for_unattributed_changes: bool,
        require_last_push_approval: bool,
        required_approving_review_count: u64,
        required_review_thread_resolution: bool,
        required_reviewers: Vec<RequiredReviewer>,
        /// Organization-owned repositories only
        dismissal_restriction: DismissalRestriction,
    }
}

impl PullRequest {
    /// Actors in `required_reviewers` and `dismissal_restriction`, with their paths relative to
    /// `rules.pull_request`.
    pub fn review_actors(&self) -> Vec<(String, &ReviewActor)> {
        let mut out = Vec::new();
        for (i, r) in self.required_reviewers.iter().flatten().enumerate() {
            out.push((format!("required_reviewers[{i}].reviewer"), &r.reviewer));
        }
        let allowed = self
            .dismissal_restriction
            .as_ref()
            .and_then(|d| d.allowed_actors.as_ref());
        for (i, actor) in allowed.into_iter().flatten().enumerate() {
            out.push((format!("dismissal_restriction.allowed_actors[{i}]"), actor));
        }
        out
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredReviewer {
    pub file_patterns: Vec<String>,
    pub minimum_approvals: u64,
    pub reviewer: ReviewActor,
}

optional_table! {
    pub struct DismissalRestriction {
        enabled: bool,
        allowed_actors: Vec<ReviewActor>,
    }
}

/// An actor in `pull_request`, written by name like a bypass actor.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewActor {
    /// `team`, `repository_role` or `user`
    #[serde(rename = "type")]
    pub actor_type: String,
    pub name: String,
}

optional_table! {
    /// Keys are shorter than the API's: `strict` is `strict_required_status_checks_policy`, and
    /// `checks` is `required_status_checks`.
    pub struct RequiredStatusChecks {
        strict: bool,
        do_not_enforce_on_create: bool,
        checks: Vec<StatusCheck>,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusCheck {
    pub context: String,
    /// App slug that must report the check
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
}

optional_table! {
    /// Parameters shared by the `*_pattern` rules.
    pub struct Pattern {
        name: String,
        negate: bool,
        /// `starts_with`, `ends_with`, `contains` or `regex`
        operator: String,
        pattern: String,
    }
}

optional_table! {
    pub struct Workflows {
        do_not_enforce_on_create: bool,
        workflows: Vec<WorkflowFile>,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    pub path: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    pub repository_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

optional_table! {
    pub struct CodeScanning {
        code_scanning_tools: Vec<CodeScanningTool>,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeScanningTool {
    pub alerts_threshold: String,
    pub security_alerts_threshold: String,
    pub tool: String,
}

optional_table! {
    pub struct CodeQuality {
        /// `errors`, `warnings`, `notes` or `all`
        severity: String,
    }
}

optional_table! {
    /// Coverage in percent
    pub struct CodeCoverage {
        minimum_coverage: f64,
        max_coverage_drop: f64,
    }
}

optional_table! {
    pub struct CopilotCodeReview {
        review_draft_pull_requests: bool,
        review_on_push: bool,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collaborator {
    #[serde(skip)]
    pub login: String,
    pub permission: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Team {
    #[serde(skip)]
    pub slug: String,
    pub permission: String,
}

/// An entry written as a table keyed by its name (`[ruleset.main]`) rather than as an element of
/// an array of tables. With `[[ruleset]]`, a header such as `[ruleset.rules]` does not say which
/// ruleset it belongs to; a keyed table names it in every header.
pub trait Keyed {
    fn key(&self) -> &str;
    fn set_key(&mut self, key: String);
}

impl Keyed for Ruleset {
    fn key(&self) -> &str {
        &self.name
    }
    fn set_key(&mut self, key: String) {
        self.name = key;
    }
}

impl Keyed for Collaborator {
    fn key(&self) -> &str {
        &self.login
    }
    fn set_key(&mut self, key: String) {
        self.login = key;
    }
}

impl Keyed for Team {
    fn key(&self) -> &str {
        &self.slug
    }
    fn set_key(&mut self, key: String) {
        self.slug = key;
    }
}

/// Maps a table of keyed entries to a `Vec` in declaration order. TOML itself rejects a key
/// declared twice, so duplicates never reach here.
mod keyed {
    use super::Keyed;
    use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
    use serde::ser::{Serialize, Serializer};
    use std::fmt;
    use std::marker::PhantomData;

    pub fn serialize<S: Serializer, T: Keyed + Serialize>(
        entries: &[T],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_map(entries.iter().map(|e| (e.key(), e)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Keyed + Deserialize<'de>>(
        d: D,
    ) -> Result<Vec<T>, D::Error> {
        struct Entries<T>(PhantomData<T>);

        impl<'de, T: Keyed + Deserialize<'de>> Visitor<'de> for Entries<T> {
            type Value = Vec<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table of entries keyed by name (e.g. [ruleset.main])")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<T>, A::Error> {
                let mut entries = Vec::new();
                while let Some((key, mut entry)) = map.next_entry::<String, T>()? {
                    entry.set_key(key);
                    entries.push(entry);
                }
                Ok(entries)
            }
        }

        d.deserialize_map(Entries(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::PARAMETERIZED_RULES;

    #[test]
    fn rules_with_parameters_are_known_to_the_schema() {
        for rule in Rules::KEYS {
            let typed = Rules::param_keys(rule).is_some() || *rule == "required_status_checks";
            assert_eq!(typed, PARAMETERIZED_RULES.contains(rule), "{rule}");
        }
    }

    #[test]
    fn rules_take_a_boolean_or_a_table() {
        let rules: Rules = toml::from_str(
            "deletion = true\nupdate = true\n[pull_request]\nrequired_approving_review_count = 1",
        )
        .unwrap();
        assert!(matches!(rules.update, Some(Rule::Enabled(true))));
        assert!(matches!(
            rules.pull_request,
            Some(Rule::Params(PullRequest {
                required_approving_review_count: Some(1),
                ..
            }))
        ));
        let text = toml::to_string(&rules).unwrap();
        assert_eq!(
            text,
            "update = true\ndeletion = true\n\n\
            [pull_request]\nrequired_approving_review_count = 1\n"
        );
    }
}
