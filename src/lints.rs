//! The lints `lint` reports besides validation: declarations GitHub accepts, but that likely
//! weaken protection (`security`) or that a stricter policy avoids (`pedantic`). Only values
//! written are checked, since an unwritten one keeps a current value lint does not know.

use serde_json::json;

use crate::diagnostic::{Applicability, Code, Diagnostic, Edit, Severity};
use crate::model::{BypassActor, Config, LintLevels, Rule, Ruleset};
use crate::validate::{entry_path, similar};

/// Ordered from the most lenient, so the strictest of several is the greatest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Allow,
    Warn,
    Deny,
}

struct Lint {
    code: Code,
    group: Option<&'static str>,
    default: Level,
}

const SECURITY: &str = "security";
const PEDANTIC: &str = "pedantic";

const fn lint(code: Code, group: Option<&'static str>, default: Level) -> Lint {
    Lint {
        code,
        group,
        default,
    }
}

const LINTS: &[Lint] = &[
    lint(Code::UnwrittenSetting, None, Level::Warn),
    lint(Code::DisabledRuleset, Some(SECURITY), Level::Warn),
    lint(Code::DefaultBranchDeletable, Some(SECURITY), Level::Warn),
    lint(
        Code::DefaultBranchForcePushable,
        Some(SECURITY),
        Level::Warn,
    ),
    lint(Code::NoRequiredApprovals, Some(PEDANTIC), Level::Allow),
    lint(Code::BypassAlways, Some(SECURITY), Level::Warn),
    lint(Code::AdminBypassAlways, Some(PEDANTIC), Level::Allow),
    lint(Code::AllActionsAllowed, Some(SECURITY), Level::Warn),
    lint(Code::ShaPinningNotRequired, Some(SECURITY), Level::Warn),
    lint(Code::WriteWorkflowPermissions, Some(SECURITY), Level::Warn),
    lint(Code::ActionsCanApprove, Some(SECURITY), Level::Warn),
    lint(Code::MutableOidcSubject, Some(SECURITY), Level::Warn),
    lint(Code::WildcardActionPattern, Some(SECURITY), Level::Warn),
    lint(Code::LenientForkPrApproval, Some(SECURITY), Level::Warn),
    lint(Code::VerifiedActionsAllowed, Some(PEDANTIC), Level::Allow),
];

/// Names that stand for several lints. `warnings` is every lint at warn when the level is given.
const GROUPS: &[&str] = &["all", SECURITY, PEDANTIC, "warnings"];

pub fn names() -> Vec<&'static str> {
    GROUPS
        .iter()
        .copied()
        .chain(LINTS.iter().map(|l| l.code.as_str()))
        .collect()
}

/// Whether a level can be given to `name`.
fn is_name(name: &str) -> bool {
    names().contains(&name)
}

/// Errors for the names in `meta.lint` that are neither a lint nor a group.
pub fn check_names(levels: &LintLevels) -> Vec<Diagnostic> {
    let names = names();
    let mut errors = Vec::new();
    for (key, list) in [
        ("allow", &levels.allow),
        ("warn", &levels.warn),
        ("deny", &levels.deny),
    ] {
        for (i, name) in list.iter().enumerate() {
            if is_name(name) {
                continue;
            }
            let path = format!("meta.lint.{key}[{i}]");
            let d = Diagnostic::error(
                Code::UnknownLint,
                format!("meta.lint.{key} entry \"{name}\" is not a lint"),
            )
            .at(&path);
            errors.push(match similar(name, &names) {
                Some(to) => d.help(format!("did you mean `{to}`?")).fix(
                    Applicability::MaybeIncorrect,
                    Edit::Set {
                        path,
                        value: json!(to),
                    },
                ),
                None => d.help(format!(
                    "write a lint code such as W0301, or one of {}",
                    GROUPS.join(" / ")
                )),
            });
        }
    }
    errors
}

/// The level of each lint, from the defaults overridden by each of `layers` in turn.
pub struct Levels(Vec<(Code, Level)>);

impl Levels {
    pub fn new(layers: &[&LintLevels]) -> Self {
        let mut levels: Vec<(Code, Level)> = LINTS.iter().map(|l| (l.code, l.default)).collect();
        for layer in layers {
            let given = |name: &str| {
                [
                    (Level::Allow, &layer.allow),
                    (Level::Warn, &layer.warn),
                    (Level::Deny, &layer.deny),
                ]
                .into_iter()
                .filter(|(_, names)| names.iter().any(|n| n == name))
                .map(|(level, _)| level)
                .max()
            };
            let mut set = |selects: &dyn Fn(&Lint, Level) -> bool, name: &str| {
                if let Some(to) = given(name) {
                    for (lint, (_, level)) in LINTS.iter().zip(&mut levels) {
                        if selects(lint, *level) {
                            *level = to;
                        }
                    }
                }
            };
            set(&|_, _| true, "all");
            for group in [SECURITY, PEDANTIC] {
                set(&|lint, _| lint.group == Some(group), group);
            }
            for lint in LINTS {
                set(&|l, _| l.code == lint.code, lint.code.as_str());
            }
            set(&|_, level| level == Level::Warn, "warnings");
        }
        Self(levels)
    }

    /// `diagnostics` with each lint dropped, or made a warning or an error, by its level.
    /// Diagnostics that are not lints pass through.
    pub fn apply(&self, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
        diagnostics
            .into_iter()
            .filter_map(|mut d| {
                match self.0.iter().find(|(code, _)| *code == d.code) {
                    Some((_, Level::Allow)) => return None,
                    Some((_, Level::Warn)) => d.severity = Severity::Warning,
                    Some((_, Level::Deny)) => d.severity = Severity::Error,
                    None => {}
                }
                Some(d)
            })
            .collect()
    }
}

/// Every lint but the unwritten settings, as warnings. [`Levels::apply`] gives them their levels.
pub fn find(config: &Config) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for ruleset in &config.rulesets {
        check_ruleset(ruleset, &mut out);
    }
    check_default_branch(config, &mut out);
    check_actions(config, &mut out);
    out
}

fn set(path: &str, value: serde_json::Value) -> Edit {
    Edit::Set {
        path: path.to_string(),
        value,
    }
}

fn check_ruleset(ruleset: &Ruleset, out: &mut Vec<Diagnostic>) {
    let base = entry_path("ruleset", &ruleset.name);
    let name = &ruleset.name;
    if ruleset.enforcement == "disabled" {
        let path = format!("{base}.enforcement");
        out.push(
            Diagnostic::warning(
                Code::DisabledRuleset,
                format!("ruleset \"{name}\" is disabled, so it enforces none of its rules"),
            )
            .at(&path)
            .help("set enforcement = \"active\", or remove the ruleset if it is no longer needed")
            .fix(Applicability::MaybeIncorrect, set(&path, json!("active"))),
        );
    }

    for (j, actor) in ruleset.bypass_actors.iter().enumerate() {
        if actor.mode == "pull_request" {
            continue;
        }
        let path = format!("{base}.bypass_actors[{j}]");
        let mode = format!("{path}.mode");
        // Admins can edit or disable the ruleset, so their bypass grants nothing they lack
        let admin = actor.actor_type == "organization_admin"
            || (actor.actor_type == "repository_role" && actor.name.as_deref() == Some("admin"));
        let message = format!(
            "ruleset \"{name}\": {} bypasses the ruleset on any push (mode = \"{}\"), not only by \
            merging a pull request",
            describe(actor),
            actor.mode
        );
        // pull_request is for branch rulesets only, and not for deploy keys
        let by_pull_request = ruleset.target == "branch" && actor.actor_type != "deploy_key";
        let d = if admin {
            Diagnostic::warning(Code::AdminBypassAlways, message).help(
                "admins can disable the ruleset anyway, but mode = \"pull_request\" keeps each \
                bypass reviewable",
            )
        } else if by_pull_request {
            Diagnostic::warning(Code::BypassAlways, message).help(
                "it can push past every rule without review; mode = \"pull_request\" lets it \
                bypass only by merging a pull request",
            )
        } else {
            Diagnostic::warning(Code::BypassAlways, message).help(
                "it can push past every rule without review; remove it unless it must push \
                past the rules",
            )
        };
        let d = d.at(&path);
        out.push(if by_pull_request {
            d.fix(
                Applicability::MaybeIncorrect,
                set(&mode, json!("pull_request")),
            )
        } else {
            d
        });
    }

    if ruleset.enforcement == "active"
        && let Some(Rule::Params(pr)) = &ruleset.rules.pull_request
        && pr.required_approving_review_count == Some(0)
    {
        out.push(
            Diagnostic::warning(
                Code::NoRequiredApprovals,
                format!("ruleset \"{name}\": pull requests can be merged without an approval"),
            )
            .at(format!(
                "{base}.rules.pull_request.required_approving_review_count"
            ))
            .help(
                "require at least one approval, unless the repository has a single maintainer, \
                who cannot approve their own pull requests",
            ),
        );
    }
}

fn describe(actor: &BypassActor) -> String {
    match &actor.name {
        Some(name) => format!("{} \"{name}\"", actor.actor_type),
        None => actor.actor_type.clone(),
    }
}

/// Whether `ruleset` applies to the default branch, as far as its patterns tell without the
/// branches of the repository: by `~DEFAULT_BRANCH`, `~ALL`, or the declared default branch.
fn covers_default_branch(config: &Config, ruleset: &Ruleset) -> bool {
    let branch = config
        .repository
        .default_branch
        .as_ref()
        .map(|branch| format!("refs/heads/{branch}"));
    let matches = |patterns: &[String]| {
        patterns
            .iter()
            .any(|p| p == "~DEFAULT_BRANCH" || p == "~ALL" || Some(p.as_str()) == branch.as_deref())
    };
    ruleset.enforcement == "active"
        && ruleset.target == "branch"
        && matches(&ruleset.conditions.include)
        && !matches(&ruleset.conditions.exclude)
}

/// Whether a ruleset enables a rule, disables it, or leaves it unwritten.
type RuleState = fn(&Ruleset) -> Option<bool>;

/// A rule the default branch should have, reported when a ruleset on it disables the rule and no
/// other enables it. An unwritten rule could be either, so it is not reported.
fn check_default_branch(config: &Config, out: &mut Vec<Diagnostic>) {
    let covering: Vec<&Ruleset> = config
        .rulesets
        .iter()
        .filter(|r| covers_default_branch(config, r))
        .collect();
    let rules: [(Code, &str, &str, RuleState); 2] = [
        (
            Code::DefaultBranchDeletable,
            "restrict_deletions",
            "can be deleted",
            |r| r.rules.deletion,
        ),
        (
            Code::DefaultBranchForcePushable,
            "block_force_pushes",
            "can be force pushed",
            |r| r.rules.non_fast_forward,
        ),
    ];
    for (code, key, what, enabled) in rules {
        if covering.iter().any(|r| enabled(r) == Some(true)) {
            continue;
        }
        for ruleset in covering.iter().filter(|r| enabled(r) == Some(false)) {
            let path = format!("{}.rules.{key}", entry_path("ruleset", &ruleset.name));
            out.push(
                Diagnostic::warning(
                    code,
                    format!(
                        "ruleset \"{}\" covers the default branch but disables {key}, and no \
                        other ruleset enables it, so the default branch {what}",
                        ruleset.name
                    ),
                )
                .at(&path)
                .help(format!("set {key} = true"))
                .fix(Applicability::MaybeIncorrect, set(&path, json!(true))),
            );
        }
    }
}

fn check_actions(config: &Config, out: &mut Vec<Diagnostic>) {
    let actions = &config.actions;
    let allowed = actions.allowed_actions.as_deref();
    if allowed == Some("all") {
        out.push(
            Diagnostic::warning(
                Code::AllActionsAllowed,
                "actions.allowed_actions = \"all\" lets workflows run any action on GitHub",
            )
            .at("actions.allowed_actions")
            .help(
                "set it to \"selected\" and list the actions to allow in actions.selected_actions",
            ),
        );
    }
    // Actions of the same owner are pinned by whoever controls the owner's repositories
    if actions.sha_pinning_required == Some(false) && allowed != Some("local_only") {
        let path = "actions.sha_pinning_required";
        out.push(
            Diagnostic::warning(
                Code::ShaPinningNotRequired,
                "actions.sha_pinning_required = false lets workflows use actions by tag or branch, \
                whose code can change under them",
            )
            .at(path)
            .help("pin the actions in the workflows to full commit SHAs, then set it to true")
            .fix(Applicability::MaybeIncorrect, set(path, json!(true))),
        );
    }
    if actions.default_workflow_permissions.as_deref() == Some("write") {
        let path = "actions.default_workflow_permissions";
        out.push(
            Diagnostic::warning(
                Code::WriteWorkflowPermissions,
                "actions.default_workflow_permissions = \"write\" gives GITHUB_TOKEN write access \
                in every workflow that does not declare permissions",
            )
            .at(path)
            .help("set it to \"read\" and declare permissions in the workflows that need more")
            .fix(Applicability::MaybeIncorrect, set(path, json!("read"))),
        );
    }
    if actions.can_approve_pull_request_reviews == Some(true) {
        let path = "actions.can_approve_pull_request_reviews";
        out.push(
            Diagnostic::warning(
                Code::ActionsCanApprove,
                "actions.can_approve_pull_request_reviews = true lets workflows approve pull \
                requests, which counts toward required approvals",
            )
            .at(path)
            .fix(Applicability::MaybeIncorrect, set(path, json!(false))),
        );
    }
    if actions.fork_pr_contributor_approval.as_deref()
        == Some("first_time_contributors_new_to_github")
    {
        out.push(
            Diagnostic::warning(
                Code::LenientForkPrApproval,
                "actions.fork_pr_contributor_approval = \"first_time_contributors_new_to_github\" \
                runs workflows of fork pull requests by first-time contributors without approval, \
                unless their account is also new",
            )
            .at("actions.fork_pr_contributor_approval")
            .help(
                "\"first_time_contributors\" or \"all_external_contributors\" requires approval \
                more often",
            ),
        );
    }
    if let Some(oidc) = &actions.oidc_subject_claim
        && oidc.use_immutable_subject == Some(false)
    {
        let path = "actions.oidc_subject_claim.use_immutable_subject";
        out.push(
            Diagnostic::warning(
                Code::MutableOidcSubject,
                "actions.oidc_subject_claim.use_immutable_subject = false names the repository in \
                the sub claim by name only, which a repository created later under the same name \
                also gets",
            )
            .at(path)
            .help("set it to true, and update the cloud trust policies that match the sub claim")
            .fix(Applicability::MaybeIncorrect, set(path, json!(true))),
        );
    }
    // selected_actions has no effect unless allowed_actions is selected
    let Some(selected) = actions
        .selected_actions
        .as_ref()
        .filter(|_| allowed.is_none_or(|allowed| allowed == "selected"))
    else {
        return;
    };
    for (i, pattern) in selected.patterns_allowed.iter().flatten().enumerate() {
        if pattern.starts_with('*') {
            let path = format!("actions.selected_actions.patterns_allowed[{i}]");
            out.push(
                Diagnostic::warning(
                    Code::WildcardActionPattern,
                    format!("pattern \"{pattern}\" allows the actions of any owner"),
                )
                .at(&path)
                .help("name the owner, as in `owner/*` or `owner/action@*`")
                .fix(Applicability::MaybeIncorrect, Edit::Remove { path }),
            );
        }
    }
    if selected.verified_allowed == Some(true) {
        out.push(
            Diagnostic::warning(
                Code::VerifiedActionsAllowed,
                "actions.selected_actions.verified_allowed = true allows the actions of every \
                GitHub Marketplace verified creator",
            )
            .at("actions.selected_actions.verified_allowed")
            .help("list the actions to allow in patterns_allowed instead"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;

    fn codes(text: &str) -> Vec<Code> {
        find(&validate::parse(text).unwrap())
            .iter()
            .map(|d| d.code)
            .collect()
    }

    fn levels(allow: &[&str], warn: &[&str], deny: &[&str]) -> LintLevels {
        let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect();
        LintLevels {
            allow: names(allow),
            warn: names(warn),
            deny: names(deny),
        }
    }

    fn level(levels: &Levels, code: Code) -> Level {
        levels.0.iter().find(|(c, _)| *c == code).unwrap().1
    }

    #[test]
    fn lints_are_listed_once_with_lint_codes() {
        for (i, lint) in LINTS.iter().enumerate() {
            assert!(lint.code.as_str().starts_with('W'), "{:?}", lint.code);
            assert!(LINTS[i + 1..].iter().all(|l| l.code != lint.code));
        }
    }

    #[test]
    fn specific_names_override_general_ones_and_layers_in_turn() {
        let defaults = Levels::new(&[]);
        assert_eq!(level(&defaults, Code::BypassAlways), Level::Warn);
        assert_eq!(level(&defaults, Code::AdminBypassAlways), Level::Allow);

        let file = levels(&["W0301"], &["pedantic"], &["security"]);
        let l = Levels::new(&[&file]);
        assert_eq!(level(&l, Code::BypassAlways), Level::Allow);
        assert_eq!(level(&l, Code::AllActionsAllowed), Level::Deny);
        assert_eq!(level(&l, Code::AdminBypassAlways), Level::Warn);

        // The strictest of the levels one name is given
        let l = Levels::new(&[&levels(&["security"], &[], &["security"])]);
        assert_eq!(level(&l, Code::AllActionsAllowed), Level::Deny);

        let cli = levels(&[], &["W0301"], &["warnings"]);
        let l = Levels::new(&[&file, &cli]);
        assert_eq!(level(&l, Code::BypassAlways), Level::Deny);
        assert_eq!(level(&l, Code::UnwrittenSetting), Level::Deny);
        assert_eq!(level(&l, Code::AdminBypassAlways), Level::Deny);
        assert_eq!(level(&l, Code::NoRequiredApprovals), Level::Deny);

        let l = Levels::new(&[&levels(&["all"], &[], &[])]);
        assert!(
            LINTS
                .iter()
                .all(|lint| level(&l, lint.code) == Level::Allow)
        );
    }

    #[test]
    fn rejects_unknown_names() {
        let errors = check_names(&levels(&["securty", "W0301"], &[], &["nope"]));
        let paths: Vec<_> = errors.iter().map(|d| d.path.as_deref().unwrap()).collect();
        assert_eq!(paths, ["meta.lint.allow[0]", "meta.lint.deny[0]"]);
        assert_eq!(
            errors[0].fix.as_ref().map(|f| &f.edit),
            Some(&set("meta.lint.allow[0]", json!("security")))
        );
    }

    #[test]
    fn bypass_by_admins_is_pedantic() {
        let text = "[ruleset.r]\ntarget = \"branch\"\nenforcement = \"active\"\nbypass_actors = [\n\
            { type = \"repository_role\", name = \"admin\" },\n\
            { type = \"integration\", name = \"renovate\" },\n\
            { type = \"integration\", name = \"ci\", mode = \"pull_request\" },\n\
            { type = \"deploy_key\", mode = \"exempt\" },\n]";
        let found = find(&validate::parse(text).unwrap());
        let found: Vec<_> = found
            .iter()
            .map(|d| (d.code, d.path.as_deref().unwrap(), d.fix.is_some()))
            .collect();
        assert_eq!(
            found,
            [
                (Code::AdminBypassAlways, "ruleset.r.bypass_actors[0]", true),
                (Code::BypassAlways, "ruleset.r.bypass_actors[1]", true),
                (Code::BypassAlways, "ruleset.r.bypass_actors[3]", false),
            ]
        );
    }

    #[test]
    fn default_branch_rules_count_every_ruleset_on_it() {
        let ruleset = |name: &str, include: &str, deletions: &str| {
            format!(
                "[ruleset.{name}]\ntarget = \"branch\"\nenforcement = \"active\"\n\
                [ruleset.{name}.conditions]\ninclude = [\"{include}\"]\n\
                [ruleset.{name}.rules]\n{deletions}\n"
            )
        };
        let off = ruleset("a", "~DEFAULT_BRANCH", "restrict_deletions = false");
        assert_eq!(codes(&off), [Code::DefaultBranchDeletable]);
        let on = ruleset("b", "~ALL", "restrict_deletions = true");
        assert!(codes(&format!("{off}{on}")).is_empty());
        // Unwritten, it keeps a state lint does not know
        assert!(codes(&ruleset("a", "~DEFAULT_BRANCH", "")).is_empty());
        assert!(
            codes(&ruleset(
                "a",
                "refs/heads/release",
                "restrict_deletions = false"
            ))
            .is_empty()
        );
        let main = format!(
            "[repository]\ndefault_branch = \"main\"\n{}",
            ruleset("a", "refs/heads/main", "block_force_pushes = false")
        );
        assert_eq!(codes(&main), [Code::DefaultBranchForcePushable]);
    }

    #[test]
    fn checks_written_actions_settings() {
        let text = "[actions]\nallowed_actions = \"all\"\nsha_pinning_required = false\n\
            default_workflow_permissions = \"write\"\ncan_approve_pull_request_reviews = true\n\
            fork_pr_contributor_approval = \"first_time_contributors_new_to_github\"\n\
            [actions.oidc_subject_claim]\nuse_immutable_subject = false\n";
        assert_eq!(
            codes(text),
            [
                Code::AllActionsAllowed,
                Code::ShaPinningNotRequired,
                Code::WriteWorkflowPermissions,
                Code::ActionsCanApprove,
                Code::LenientForkPrApproval,
                Code::MutableOidcSubject,
            ]
        );
        let text = "[actions]\nallowed_actions = \"selected\"\n[actions.selected_actions]\n\
            verified_allowed = true\npatterns_allowed = [\"octo/*\", \"*/*@*\"]\n";
        assert_eq!(
            codes(text),
            [Code::WildcardActionPattern, Code::VerifiedActionsAllowed]
        );
        assert!(codes(&text.replace("\"selected\"", "\"local_only\"")).is_empty());
    }
}
