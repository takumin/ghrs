//! Schema of `.ghrs.toml`.
//!
//! Field declaration order is the TOML output order as-is. Because TOML requires scalars to
//! come before tables, each struct is ordered scalars -> arrays -> tables.
//!
//! Unknown keys are rejected on load. If a misspelled key were silently ignored, plan would
//! report "no diff" while the setting the user meant to declare is never applied.

use serde::{Deserialize, Serialize};
use toml::Table;

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub repository: Repository,
    #[serde(rename = "ruleset", default, skip_serializing_if = "Vec::is_empty")]
    pub rulesets: Vec<Ruleset>,
    #[serde(
        rename = "collaborator",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub collaborators: Vec<Collaborator>,
    #[serde(rename = "team", default, skip_serializing_if = "Vec::is_empty")]
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
    pub name: String,
    pub target: String,
    pub enforcement: String,
    #[serde(default)]
    pub conditions: Conditions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass_actors: Vec<BypassActor>,
    #[serde(default)]
    pub rules: Table,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collaborator {
    pub login: String,
    pub permission: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Team {
    pub slug: String,
    pub permission: String,
}
