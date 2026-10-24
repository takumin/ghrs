//! Schema of `.ghrs.toml`.
//!
//! Field declaration order is the TOML output order as-is. Because TOML requires scalars to
//! come before tables, each struct is ordered scalars -> arrays -> tables.
//!
//! Unknown keys are rejected on load. If a misspelled key were silently ignored, plan would
//! report "no diff" while the setting the user meant to declare is never applied.
//!
//! The JSON Schema printed by `ghrs schema` is derived from these types. Doc comments become its
//! descriptions, and `x-github-*` keywords tie the ruleset types to GitHub's REST API
//! description:
//! - `x-github-source`: the schema in the REST API description that the type or rule follows
//! - `x-github-api-name`: the API's name for a key ghrs names differently or writes by name
//! - `x-github-observed`: a key the API returns but the REST API description does not document

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::schema::{self, rest_api_schema};

/// Declares a table whose fields are all optional, and lists its keys in `KEYS`. import uses
/// `KEYS` to tell what ghrs knows from what GitHub added later, and plan to find the keys left
/// unwritten.
macro_rules! optional_table {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $($(#[$field_meta:meta])* $field:ident: $ty:ty,)*
        }
    ) => {
        #[derive(Serialize, Deserialize, Default, JsonSchema)]
        #[serde(deny_unknown_fields)]
        $(#[$meta])*
        pub struct $name {
            $(
                $(#[$field_meta])*
                #[serde(default, skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }

        impl $name {
            pub const KEYS: &[&str] = &[$(stringify!($field)),*];
        }
    };
}

/// Settings of one GitHub repository, declared for ghrs.
#[derive(Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The target repository, used when --repo is not given, and the settings left unchanged on
    /// purpose
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
    /// Repository settings. Only the fields written are compared
    #[serde(default)]
    pub repository: Repository,
    /// GitHub Actions settings. Only the fields written are compared
    #[serde(default, skip_serializing_if = "Actions::is_empty")]
    pub actions: Actions,
    /// Rulesets keyed by name (`[ruleset.main]`). Rulesets not declared are kept unless plan /
    /// apply run with --prune
    #[serde(
        rename = "ruleset",
        with = "keyed",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    #[schemars(with = "BTreeMap<String, Ruleset>")]
    pub rulesets: Vec<Ruleset>,
    /// Direct collaborators keyed by login. Collaborators not declared are kept unless plan /
    /// apply run with --prune
    #[serde(
        rename = "collaborator",
        with = "keyed",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    #[schemars(with = "BTreeMap<String, Collaborator>")]
    pub collaborators: Vec<Collaborator>,
    /// Teams keyed by slug (Organization-owned repositories only). Teams not declared are kept
    /// unless plan / apply run with --prune
    #[serde(
        rename = "team",
        with = "keyed",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    #[schemars(with = "BTreeMap<String, Team>")]
    pub teams: Vec<Team>,
}

/// The target repository, and the settings ghrs leaves unchanged on purpose. --repo takes
/// precedence over `owner` and `repo`, and without either, the target is taken from the git
/// remote.
#[derive(Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// User or Organization that owns the repository
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Repository name, without the owner
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Key paths of settings to leave unchanged without declaring them, such as `actions` or
    /// `ruleset.main.rules.pull_request.required_reviewers`. A table covers every key in it.
    /// plan reports the settings ghrs knows but neither the file nor this list mentions, and
    /// --strict rejects them
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmanaged: Vec<String>,
}

impl Meta {
    fn is_empty(&self) -> bool {
        self.owner.is_none() && self.repo.is_none() && self.unmanaged.is_empty()
    }
}

optional_table! {
    /// Repository settings, with the names of `PATCH /repos/{owner}/{repo}`. Only the fields that
    /// are written are compared. Unwritten fields are left untouched.
    pub struct Repository {
        /// A short description of the repository
        description: String,
        /// A URL with more information about the repository
        homepage: String,
        /// Rulesets are not available on private repositories on the free plan
        #[schemars(extend("enum" = schema::VISIBILITIES))]
        visibility: String,
        /// Enables issues
        has_issues: bool,
        /// Enables the wiki
        has_wiki: bool,
        /// Enables projects
        has_projects: bool,
        /// Enables discussions. `PATCH /repos/{owner}/{repo}` does not document it; the repository
        /// object documents it as whether discussions are enabled
        has_discussions: bool,
        /// Allows merging pull requests with a merge commit
        allow_merge_commit: bool,
        /// Allows squash-merging pull requests
        allow_squash_merge: bool,
        /// Allows rebase-merging pull requests
        allow_rebase_merge: bool,
        /// Allows auto-merge on pull requests
        allow_auto_merge: bool,
        /// Deletes head branches automatically when pull requests are merged
        delete_branch_on_merge: bool,
        /// Default branch. The branch must already exist
        default_branch: String,
        /// Set with `PUT /repos/{owner}/{repo}/topics`
        topics: Vec<String>,
    }
}

optional_table! {
    /// GitHub Actions settings of the repository, with the names of
    /// `PUT /repos/{owner}/{repo}/actions/permissions` and `.../actions/permissions/workflow`.
    /// Only the fields that are written are compared. Unwritten fields are left untouched. The
    /// owning Organization's policy can prevent a change, and GitHub then returns a 409.
    pub struct Actions {
        /// Enables GitHub Actions
        #[schemars(extend("x-github-source" = rest_api_schema("actions-enabled")))]
        enabled: bool,
        /// Actions and reusable workflows allowed to run. `local_only`: only those in
        /// repositories of the same owner. `selected`: those and the ones `selected_actions`
        /// allows. `all`: any
        #[schemars(extend(
            "enum" = schema::ALLOWED_ACTIONS,
            "x-github-source" = rest_api_schema("allowed-actions")
        ))]
        allowed_actions: String,
        /// Actions must be pinned to a full-length commit SHA
        #[schemars(extend("x-github-source" = rest_api_schema("sha-pinning-required")))]
        sha_pinning_required: bool,
        /// Permissions granted to `GITHUB_TOKEN` when a workflow does not declare `permissions`
        #[schemars(extend(
            "enum" = schema::WORKFLOW_PERMISSIONS,
            "x-github-source" = rest_api_schema("actions-default-workflow-permissions")
        ))]
        default_workflow_permissions: String,
        /// GitHub Actions can approve pull requests
        #[schemars(extend(
            "x-github-source" = rest_api_schema("actions-can-approve-pull-request-reviews")
        ))]
        can_approve_pull_request_reviews: bool,
        /// Which fork pull requests need a maintainer's approval before their workflows run.
        /// `all_external_contributors`: those of anyone who is not a collaborator or
        /// Organization member. `first_time_contributors`: those of users who have not
        /// contributed before. `first_time_contributors_new_to_github`: those of first-time
        /// contributors who are also new to GitHub. Public repositories only. Set with
        /// `PUT /repos/{owner}/{repo}/actions/permissions/fork-pr-contributor-approval`
        #[schemars(extend(
            "enum" = schema::FORK_PR_APPROVAL_POLICIES,
            "x-github-api-name" = "approval_policy",
            "x-github-source" = rest_api_schema("actions-fork-pr-contributor-approval")
        ))]
        fork_pr_contributor_approval: String,
        /// What `allowed_actions = "selected"` allows besides the owner's actions, which it
        /// requires. Set with `PUT /repos/{owner}/{repo}/actions/permissions/selected-actions`
        selected_actions: SelectedActions,
        /// The `sub` claim of the OIDC tokens workflows get. Set with
        /// `PUT /repos/{owner}/{repo}/actions/oidc/customization/sub`
        oidc_subject_claim: OidcSubjectClaim,
    }
}

impl Actions {
    pub fn is_empty(&self) -> bool {
        serde_json::to_value(self).is_ok_and(|v| v.as_object().is_some_and(|m| m.is_empty()))
    }
}

optional_table! {
    /// Actions and reusable workflows allowed with `allowed_actions = "selected"`. Keys not
    /// written keep their current values.
    #[schemars(extend("x-github-source" = rest_api_schema("selected-actions")))]
    pub struct SelectedActions {
        /// Allows actions owned by GitHub, such as those in the `actions` Organization
        github_owned_allowed: bool,
        /// Allows actions by GitHub Marketplace verified creators
        verified_allowed: bool,
        /// Patterns of actions and reusable workflows to allow, such as `monalisa/octocat@*`,
        /// `monalisa/octocat@v2` or `monalisa/*`. GitHub applies them to public repositories only
        patterns_allowed: Vec<String>,
    }
}

optional_table! {
    /// The template GitHub makes the `sub` claim from. Keys not written keep their current
    /// values.
    #[schemars(extend("x-github-source" = rest_api_schema("oidc-custom-sub-repo")))]
    pub struct OidcSubjectClaim {
        /// Uses the default template instead of `include_claim_keys`
        use_default: bool,
        /// Uses a `sub` claim that names the owner and the repository by ID as well, so that it
        /// does not match a renamed or recreated repository
        use_immutable_subject: bool,
        /// Claims the `sub` claim is made of, in this order, such as `repo` or `context`. Needs
        /// `use_default = false`
        include_claim_keys: Vec<String>,
    }
}

/// A ruleset of the repository. Rulesets inherited from the Organization are not managed.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema("repository-ruleset")))]
pub struct Ruleset {
    #[serde(skip)]
    pub name: String,
    /// Push rulesets are Team / Enterprise only
    #[schemars(extend("enum" = schema::TARGETS))]
    pub target: String,
    /// `evaluate` is Enterprise only
    #[schemars(extend("enum" = schema::ENFORCEMENTS))]
    pub enforcement: String,
    /// Refs the ruleset applies to. Omitted, it applies to no ref
    #[serde(default)]
    pub conditions: Conditions,
    /// Actors that may bypass the ruleset. Actors not listed lose their bypass, so omitting this
    /// removes every bypass actor
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass_actors: Vec<BypassActor>,
    /// Rules not written keep their current state; `false` disables a rule
    #[serde(default)]
    pub rules: Rules,
}

/// Refs the ruleset applies to (`conditions.ref_name` in the API). Patterns start with
/// `refs/heads/` for branch rulesets and `refs/tags/` for tag rulesets, except `~ALL` and (branch
/// rulesets only) `~DEFAULT_BRANCH`.
#[derive(Serialize, Deserialize, Default, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema("repository-ruleset-conditions")))]
pub struct Conditions {
    /// Ref patterns the ruleset applies to (fnmatch syntax)
    #[serde(default)]
    #[schemars(extend("x-github-api-name" = "ref_name.include"))]
    pub include: Vec<String>,
    /// Ref patterns excluded even when `include` matches them (fnmatch syntax)
    #[serde(default)]
    #[schemars(extend("x-github-api-name" = "ref_name.exclude"))]
    pub exclude: Vec<String>,
}

/// An actor that may bypass the ruleset, written by name. ghrs resolves the name to the API's
/// `actor_id`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema("repository-ruleset-bypass-actor")))]
pub struct BypassActor {
    /// `team` and `organization_admin` are Organization-owned repositories only
    #[serde(rename = "type")]
    #[schemars(extend("enum" = schema::actor_type_names(), "x-github-api-name" = "actor_type"))]
    pub actor_type: String,
    /// App slug (`self` for the App ghrs runs as), team slug, or role name (`maintain`, `write`
    /// or `admin`). Only `integration` accepts a numeric App ID, for private Apps of someone else.
    /// Omitted for `organization_admin` and `deploy_key`, which refer to the whole type and have
    /// no ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("x-github-api-name" = "actor_id"))]
    pub name: Option<String>,
    /// `pull_request` is for branch rulesets only, and not for `deploy_key`
    #[serde(default = "default_bypass_mode")]
    #[schemars(extend("enum" = schema::BYPASS_MODES, "x-github-api-name" = "bypass_mode"))]
    pub mode: String,
}

fn default_bypass_mode() -> String {
    "always".to_string()
}

optional_table! {
    /// Rules of a ruleset. Rules not written here keep their current state; `false` disables a
    /// rule.
    ///
    /// Rules GitHub has but this table lacks cannot be declared. plan keeps them at their current
    /// values as well, so a rule GitHub adds later is not deleted.
    //
    // Fields are named by the API's rule type, so `KEYS` lists rule types. The renames follow
    // `schema::RULE_NAMES`.
    #[schemars(
        extend("x-github-source" = rest_api_schema("repository-rule")),
        transform = rule_sources
    )]
    pub struct Rules {
        /// Only bypass actors can create matching refs
        #[serde(rename = "restrict_creations")]
        creation: bool,
        /// Only bypass actors can update matching refs
        #[serde(rename = "restrict_updates")]
        update: Rule<Update>,
        /// Only bypass actors can delete matching refs
        #[serde(rename = "restrict_deletions")]
        deletion: bool,
        /// Prevents merge commits from being pushed to matching refs
        required_linear_history: bool,
        merge_queue: Rule<MergeQueue>,
        required_deployments: Rule<RequiredDeployments>,
        /// Commits pushed to matching refs must have verified signatures
        required_signatures: bool,
        pull_request: Rule<PullRequest>,
        required_status_checks: Rule<RequiredStatusChecks>,
        /// Only bypass actors can force push to matching refs
        #[serde(rename = "block_force_pushes")]
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
        /// Added or changed dependencies must comply with the Organization's license policy
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

    /// The rules a ruleset with `target` can have, by rule type. Rules for the other target, and
    /// on a personal repository those for Organizations only, are left out.
    pub fn applicable(target: &str, owned_by_org: bool) -> impl Iterator<Item = &'static str> {
        let other_target = match target {
            "branch" => schema::TAG_ONLY_RULES,
            "tag" => schema::BRANCH_ONLY_RULES,
            _ => &[],
        };
        Self::KEYS.iter().copied().filter(move |rule| {
            !other_target.contains(rule) && (owned_by_org || !schema::ORG_ONLY_RULES.contains(rule))
        })
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

/// Adds to each rule where the REST API description defines it, and the API's name of a rule
/// ghrs names differently.
fn rule_sources(schema: &mut Schema) {
    if let Some(Json::Object(rules)) = schema.get_mut("properties") {
        for (rule, property) in rules {
            if let Some(property) = property.as_object_mut() {
                let api = schema::rule_to_api(rule);
                property.insert(
                    "x-github-source".to_string(),
                    schema::rest_api_rule(api).into(),
                );
                if api != rule {
                    property.insert("x-github-api-name".to_string(), api.into());
                }
            }
        }
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

impl<P: JsonSchema> JsonSchema for Rule<P> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        format!("Rule_{}", P::schema_name()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "`true` / `false`, or a table of parameters. `true` keeps the current \
                parameters (the defaults on creation), and parameters not written in a table keep \
                their current values",
            "anyOf": [{ "type": "boolean" }, generator.subschema_for::<P>()],
        })
    }
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
        /// The branch can still pull changes from its upstream repository
        update_allows_fetch_and_merge: bool,
    }
}

optional_table! {
    pub struct MergeQueue {
        /// Minutes a required status check has to report a conclusion before it is assumed to
        /// have failed
        check_response_timeout_minutes: u64,
        /// `ALLGREEN`: the merge commit for each pull request in a group must pass the required
        /// checks. `HEADGREEN`: only the commit at the head of the group must pass them
        #[schemars(extend("enum" = schema::MERGE_QUEUE_GROUPING))]
        grouping_strategy: String,
        /// Number of queued pull requests requesting checks and workflow runs at the same time
        max_entries_to_build: u64,
        /// Maximum number of pull requests merged together in a group
        max_entries_to_merge: u64,
        /// Method used to merge queued pull requests
        #[schemars(extend("enum" = schema::MERGE_QUEUE_METHODS))]
        merge_method: String,
        /// Minimum number of pull requests merged together in a group
        min_entries_to_merge: u64,
        /// Minutes to wait for `min_entries_to_merge` after the first pull request is queued.
        /// After that, a smaller group is merged
        min_entries_to_merge_wait_minutes: u64,
    }
}

optional_table! {
    pub struct RequiredDeployments {
        /// Environments that must be deployed to successfully before a branch can be merged
        required_deployment_environments: Vec<String>,
    }
}

optional_table! {
    pub struct PullRequest {
        /// Merge methods allowed for pull requests. At least one is required
        #[schemars(extend("items" = { "type": "string", "enum": schema::MERGE_METHODS }))]
        allowed_merge_methods: Vec<String>,
        /// New reviewable commits dismiss previous approvals
        dismiss_stale_reviews_on_push: bool,
        /// Pull requests that modify files with a designated code owner need an approval from
        /// one
        require_code_owner_review: bool,
        /// Requires one more approval than `required_approving_review_count` on pull requests
        /// Copilot opens under its own app identity, not on behalf of a person. No effect when zero
        /// approvals are required. GitHub enables it when omitted. Not in the REST API
        /// description; the meaning is from GitHub's ruleset docs (public preview)
        #[schemars(extend("x-github-observed" = true))]
        require_extra_approval_for_unattributed_changes: bool,
        /// The most recent reviewable push must be approved by someone other than its pusher
        require_last_push_approval: bool,
        /// Number of approving reviews required before a pull request can be merged
        required_approving_review_count: u64,
        /// All conversations on code must be resolved before a pull request can be merged
        required_review_thread_resolution: bool,
        /// Teams that must review changes to matching files
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

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema(
    "repository-rule-params-required-reviewer-configuration"
)))]
pub struct RequiredReviewer {
    /// Pull requests changing files that match these patterns (fnmatch syntax) need the
    /// reviewer's approval
    pub file_patterns: Vec<String>,
    /// Approvals required from the reviewer. `0` adds the team to the pull request without
    /// requiring its approval
    pub minimum_approvals: u64,
    /// The team that reviews matching files
    pub reviewer: ReviewActor,
}

impl RequiredReviewer {
    /// Keys as written in TOML and the API. import uses them to tell what ghrs knows from what
    /// GitHub added later
    pub const KEYS: &[&str] = &["file_patterns", "minimum_approvals", "reviewer"];
}

optional_table! {
    /// Organization-owned repositories only. `enabled` is required unless the ruleset already has
    /// a restriction, whose keys not written here keep their current values
    #[schemars(extend("x-github-source" = rest_api_schema(
        "repository-rule-params-dismissal-restriction"
    )))]
    pub struct DismissalRestriction {
        /// Restricts review dismissal to `allowed_actors`
        enabled: bool,
        /// Actors allowed to dismiss pull request reviews
        allowed_actors: Vec<ReviewActor>,
    }
}

/// An actor in `pull_request`, written by name like a bypass actor. ghrs resolves the name to the
/// API's `id`. `required_reviewers` accepts only `team`, and a `repository_role` in
/// `dismissal_restriction` must be `admin`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewActor {
    /// The kind of actor `name` refers to
    #[serde(rename = "type")]
    #[schemars(extend("enum" = schema::review_actor_type_names()))]
    pub actor_type: String,
    /// Team slug, role name, user login, or App slug (`self` for the App ghrs runs as). An App
    /// is sent as its installation ID in the Organization
    #[schemars(extend("x-github-api-name" = "id"))]
    pub name: String,
}

optional_table! {
    /// Keys differ from the API's: `require_up_to_date` is
    /// `strict_required_status_checks_policy`, and `checks` is `required_status_checks`.
    pub struct RequiredStatusChecks {
        /// Branches must be up to date with the base branch before merging
        #[schemars(extend("x-github-api-name" = "strict_required_status_checks_policy"))]
        require_up_to_date: bool,
        /// Refs can be created even when a check would otherwise prohibit it
        do_not_enforce_on_create: bool,
        /// Status checks that must pass
        #[schemars(extend("x-github-api-name" = "required_status_checks"))]
        checks: Vec<StatusCheck>,
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema(
    "repository-rule-params-status-check-configuration"
)))]
pub struct StatusCheck {
    /// Name of the status check (the job name for GitHub Actions)
    pub context: String,
    /// Slug of the App that must report the check. ghrs resolves it to the App ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("x-github-api-name" = "integration_id"))]
    pub integration: Option<String>,
}

optional_table! {
    /// Parameters shared by the `*_pattern` rules.
    pub struct Pattern {
        /// How the rule appears in GitHub's settings
        name: String,
        /// Fails the rule when the pattern matches, instead of when it does not
        negate: bool,
        /// How `pattern` is compared with the value
        #[schemars(extend("enum" = schema::PATTERN_OPERATORS))]
        operator: String,
        /// The string or regular expression to match
        pattern: String,
    }
}

optional_table! {
    pub struct Workflows {
        /// Refs can be created even when a workflow would otherwise prohibit it
        do_not_enforce_on_create: bool,
        /// Workflows that must pass
        workflows: Vec<WorkflowFile>,
    }
}

/// `repository_id` is a numeric ID, which ghrs cannot resolve from a name yet, so validation
/// rejects it: a workflow file cannot be declared. `workflows = true` keeps the current ones.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema(
    "repository-rule-params-workflow-file-reference"
)))]
pub struct WorkflowFile {
    /// Path of the workflow file
    pub path: String,
    /// Branch or tag of the workflow file to use
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// ID of the repository where the workflow is defined
    pub repository_id: u64,
    /// Commit SHA of the workflow file to use
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

impl WorkflowFile {
    /// Keys as written in TOML and the API. import uses them to tell what ghrs knows from what
    /// GitHub added later
    pub const KEYS: &[&str] = &["path", "ref", "repository_id", "sha"];
}

optional_table! {
    pub struct CodeScanning {
        /// Tools that must provide code scanning results
        code_scanning_tools: Vec<CodeScanningTool>,
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-github-source" = rest_api_schema(
    "repository-rule-params-code-scanning-tool"
)))]
pub struct CodeScanningTool {
    /// Alert severity at which results block a ref update
    #[schemars(extend("enum" = schema::ALERTS_THRESHOLDS))]
    pub alerts_threshold: String,
    /// Security severity at which results block a ref update
    #[schemars(extend("enum" = schema::SECURITY_ALERTS_THRESHOLDS))]
    pub security_alerts_threshold: String,
    /// Name of the code scanning tool
    pub tool: String,
}

impl CodeScanningTool {
    /// Keys as written in TOML and the API. import uses them to tell what ghrs knows from what
    /// GitHub added later
    pub const KEYS: &[&str] = &["alerts_threshold", "security_alerts_threshold", "tool"];
}

optional_table! {
    pub struct CodeQuality {
        /// Lowest severity of code quality findings that must be resolved before merging
        #[schemars(extend("enum" = schema::CODE_QUALITY_SEVERITIES))]
        severity: String,
    }
}

optional_table! {
    /// Coverage in percent
    pub struct CodeCoverage {
        /// Pull requests with line coverage below this are blocked
        minimum_coverage: f64,
        /// Pull requests whose line coverage drops more than this many percentage points from the
        /// default branch are blocked
        max_coverage_drop: f64,
    }
}

optional_table! {
    pub struct CopilotCodeReview {
        /// Copilot also reviews draft pull requests
        review_draft_pull_requests: bool,
        /// Copilot reviews each new push to a pull request
        review_on_push: bool,
    }
}

/// The repository owner cannot be declared. On a personal repository, only `push` is available.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Collaborator {
    #[serde(skip)]
    pub login: String,
    /// Repository role granted to the user
    #[schemars(extend("enum" = schema::PERMISSIONS))]
    pub permission: String,
}

/// Access of one team to the repository.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Team {
    #[serde(skip)]
    pub slug: String,
    /// Repository role granted to the team
    #[schemars(extend("enum" = schema::PERMISSIONS))]
    pub permission: String,
}

/// JSON Schema of the declaration file, for editors (e.g. taplo) and coding agents.
pub fn json_schema() -> Json {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<Config>()
        .to_value();
    without_null(&mut schema);
    schema["title"] = "ghrs declaration file (.ghrs.toml)".into();
    schema["x-github-rest-api-description"] = schema::REST_API_DESCRIPTION.into();
    schema
}

/// Drop `null` from the schema. TOML has no null, so an omitted key is how an `Option` is unset.
fn without_null(v: &mut Json) {
    match v {
        Json::Object(map) => {
            if map.get("default") == Some(&Json::Null) {
                map.remove("default");
            }
            if let Some(Json::Array(types)) = map.get_mut("type") {
                types.retain(|t| t != "null");
                if let [only] = types.as_slice() {
                    let only = only.clone();
                    map.insert("type".to_string(), only);
                }
            }
            if let Some(Json::Array(any_of)) = map.get_mut("anyOf") {
                any_of.retain(|s| s.get("type").is_none_or(|t| t != "null"));
                if let [only] = any_of.as_slice()
                    && let Json::Object(only) = only.clone()
                {
                    map.remove("anyOf");
                    map.extend(only);
                }
            }
            map.values_mut().for_each(without_null);
        }
        Json::Array(items) => items.iter_mut().for_each(without_null),
        _ => {}
    }
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
    use std::collections::BTreeSet;

    #[test]
    fn applicable_rules_follow_target_and_owner() {
        let branch: Vec<_> = Rules::applicable("branch", false).collect();
        assert!(branch.contains(&"copilot_code_review"));
        assert!(!branch.contains(&"tag_name_pattern"));
        assert!(!branch.contains(&"merge_queue"));
        let tag: Vec<_> = Rules::applicable("tag", true).collect();
        assert!(tag.contains(&"tag_name_pattern"));
        assert!(!tag.contains(&"pull_request"));
    }

    /// The keys import keeps in nested tables are the ones the types serialize
    #[test]
    fn nested_keys_match_fields() {
        let keys = |v: serde_json::Value| -> Vec<String> {
            let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        let sorted = |k: &[&str]| -> Vec<String> {
            let mut k: Vec<String> = k.iter().map(|s| s.to_string()).collect();
            k.sort();
            k
        };
        let reviewer = RequiredReviewer {
            file_patterns: vec![],
            minimum_approvals: 0,
            reviewer: ReviewActor {
                actor_type: String::new(),
                name: String::new(),
            },
        };
        let tool = CodeScanningTool {
            alerts_threshold: String::new(),
            security_alerts_threshold: String::new(),
            tool: String::new(),
        };
        let workflow = WorkflowFile {
            path: String::new(),
            git_ref: Some(String::new()),
            repository_id: 0,
            sha: Some(String::new()),
        };
        assert_eq!(
            keys(serde_json::to_value(&reviewer).unwrap()),
            sorted(RequiredReviewer::KEYS)
        );
        assert_eq!(
            keys(serde_json::to_value(&tool).unwrap()),
            sorted(CodeScanningTool::KEYS)
        );
        assert_eq!(
            keys(serde_json::to_value(&workflow).unwrap()),
            sorted(WorkflowFile::KEYS)
        );
    }

    /// Checks `value` against the subset of JSON Schema that [`json_schema`] produces, and
    /// collects the paths that do not match.
    fn mismatches(
        root: &Json,
        schema: &Json,
        value: &toml::Value,
        path: &str,
        out: &mut Vec<String>,
    ) {
        let schema = match schema["$ref"].as_str() {
            Some(r) => &root["$defs"][r.trim_start_matches("#/$defs/")],
            None => schema,
        };
        if let Some(any_of) = schema["anyOf"].as_array() {
            let fits = any_of.iter().any(|branch| {
                let mut errors = Vec::new();
                mismatches(root, branch, value, path, &mut errors);
                errors.is_empty()
            });
            if !fits {
                out.push(format!("{path}: matches no branch of anyOf"));
            }
            return;
        }
        if let Some(allowed) = schema["enum"].as_array()
            && !allowed
                .iter()
                .any(|a| Some(a.as_str()) == Some(value.as_str()))
        {
            out.push(format!("{path}: {value} is not in the enum"));
        }
        let fits = match (schema["type"].as_str(), value) {
            (None, _) => true,
            (Some("string"), toml::Value::String(_)) => true,
            (Some("boolean"), toml::Value::Boolean(_)) => true,
            (Some("integer"), toml::Value::Integer(_)) => true,
            (Some("number"), toml::Value::Integer(_) | toml::Value::Float(_)) => true,
            (Some("array"), toml::Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    mismatches(root, &schema["items"], item, &format!("{path}[{i}]"), out);
                }
                true
            }
            (Some("object"), toml::Value::Table(table)) => {
                for (key, item) in table {
                    let at = format!("{path}.{key}");
                    match (&schema["properties"][key], &schema["additionalProperties"]) {
                        (Json::Null, Json::Bool(false)) => out.push(format!("{at}: unknown key")),
                        (Json::Null, Json::Null) => {}
                        (Json::Null, additional) => mismatches(root, additional, item, &at, out),
                        (property, _) => mismatches(root, property, item, &at, out),
                    }
                }
                for key in schema["required"].as_array().into_iter().flatten() {
                    if !table.contains_key(key.as_str().unwrap_or_default()) {
                        out.push(format!("{path}: missing {key}"));
                    }
                }
                true
            }
            _ => false,
        };
        if !fits {
            out.push(format!("{path}: {value} is not of type {}", schema["type"]));
        }
    }

    fn schema_mismatches(text: &str) -> Vec<String> {
        let schema = json_schema();
        let value: toml::Value = toml::from_str(text).unwrap();
        let mut out = Vec::new();
        mismatches(&schema, &schema, &value, "", &mut out);
        out
    }

    /// The JSON Schema and the parser agree on the README sample
    #[test]
    fn json_schema_accepts_what_ghrs_accepts() {
        let readme = include_str!("../README.md");
        let sample = readme
            .split("## Declaration file")
            .nth(1)
            .and_then(|s| s.split("```toml\n").nth(1))
            .and_then(|s| s.split("```").next())
            .unwrap();
        assert!(crate::validate::parse(sample).is_ok());
        assert_eq!(schema_mismatches(sample), Vec::<String>::new());

        let rules = "[ruleset.r]\ntarget = \"branch\"\nenforcement = \"active\"\n\
            [ruleset.r.rules]\nrestrict_updates = true\n[ruleset.r.rules.pull_request]\n\
            dismissal_restriction = { enabled = true, allowed_actors = [\
                { type = \"integration\", name = \"self\" }] }\n\
            [ruleset.r.rules.code_coverage]\nminimum_coverage = 80\n";
        assert!(crate::validate::parse(rules).is_ok());
        assert_eq!(schema_mismatches(rules), Vec::<String>::new());
    }

    #[test]
    fn json_schema_rejects_what_ghrs_rejects() {
        let ruleset = |body: &str| {
            format!("[ruleset.r]\ntarget = \"branch\"\nenforcement = \"active\"\n{body}")
        };
        for (text, mismatch) in [
            (
                "[repository]\nallow_rebase = false",
                ".repository.allow_rebase: unknown key",
            ),
            (
                &ruleset("[ruleset.r.rules]\nblock_force_push = true"),
                ".ruleset.r.rules.block_force_push: unknown key",
            ),
            (
                &ruleset("[ruleset.r.rules]\npull_request = 1"),
                ".ruleset.r.rules.pull_request: matches no branch of anyOf",
            ),
            (
                "[collaborator.octocat]\npermission = \"write\"",
                ".collaborator.octocat.permission: \"write\" is not in the enum",
            ),
            (
                "[ruleset.r]\ntarget = \"branch\"",
                ".ruleset.r: missing \"enforcement\"",
            ),
            (
                "[actions]\nallowed_actions = \"any\"",
                ".actions.allowed_actions: \"any\" is not in the enum",
            ),
            (
                "[actions.selected_actions]\npatterns = []",
                ".actions.selected_actions.patterns: unknown key",
            ),
        ] {
            assert_eq!(schema_mismatches(text), [mismatch], "{text}");
        }
    }

    #[test]
    fn json_schema_has_no_null_and_sources_every_rule() {
        let schema = json_schema();
        assert!(!schema.to_string().contains("\"null\""));
        let rules = schema["$defs"]["Rules"]["properties"].as_object().unwrap();
        assert_eq!(rules.len(), Rules::KEYS.len());
        for (rule, property) in rules {
            assert_eq!(
                property["x-github-source"],
                Json::from(schema::rest_api_rule(schema::rule_to_api(rule))),
                "{rule}"
            );
        }
    }

    /// `import` tells readers to take key meanings from the schema, so every key has one
    #[test]
    fn json_schema_describes_every_property() {
        fn undescribed(schema: &Json, path: &str, out: &mut Vec<String>) {
            match schema {
                Json::Object(map) => {
                    for (key, property) in map
                        .get("properties")
                        .and_then(Json::as_object)
                        .into_iter()
                        .flatten()
                    {
                        if property.get("description").is_none() {
                            out.push(format!("{path}.{key}"));
                        }
                    }
                    for (key, child) in map {
                        undescribed(child, &format!("{path}/{key}"), out);
                    }
                }
                Json::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        undescribed(item, &format!("{path}[{i}]"), out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        undescribed(&json_schema(), "", &mut out);
        assert_eq!(out, Vec::<String>::new());
    }

    #[test]
    fn rules_with_parameters_are_known_to_the_schema() {
        for rule in Rules::KEYS {
            let typed = Rules::param_keys(rule).is_some() || *rule == "required_status_checks";
            assert_eq!(typed, PARAMETERIZED_RULES.contains(rule), "{rule}");
        }
    }

    /// The serde renames of [`Rules`] and `schema::RULE_NAMES` agree
    #[test]
    fn rules_are_written_by_their_toml_names() {
        let text: String = Rules::KEYS
            .iter()
            .map(|rule| format!("{} = true\n", schema::rule_to_toml(rule)))
            .collect();
        let rules: Rules = toml::from_str(&text).unwrap();
        let entries = rules.entries();
        let keys: BTreeSet<&str> = entries.keys().map(|k| schema::rule_to_api(k)).collect();
        assert_eq!(keys, Rules::KEYS.iter().copied().collect());
    }

    #[test]
    fn rules_take_a_boolean_or_a_table() {
        let rules: Rules = toml::from_str(
            "restrict_deletions = true\nrestrict_updates = true\n\
            [pull_request]\nrequired_approving_review_count = 1",
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
            "restrict_updates = true\nrestrict_deletions = true\n\n\
            [pull_request]\nrequired_approving_review_count = 1\n"
        );
    }
}
