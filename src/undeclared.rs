//! Settings ghrs knows but the declaration file leaves unwritten. plan keeps them at their current
//! values, so a key forgotten, or one a newer ghrs added, would go unmanaged without notice. They
//! are reported unless `meta.unmanaged` lists them, and --strict rejects them.

use serde_json::Value as Json;

use crate::diagnostic::{Applicability, Code, Diagnostic, Edit};
use crate::model::{
    Actions, Config, DismissalRestriction, OidcSubjectClaim, Repository, RequiredStatusChecks,
    Rules, SelectedActions,
};
use crate::plan::Scope;
use crate::schema::rule_to_api;
use crate::validate::entry_path;

/// The settings left unwritten and not covered by `meta.unmanaged`, as key paths. A table with no
/// key written is reported as the table. Also returns an error for each entry of `meta.unmanaged`
/// that covers none of them, since it is either stale or a mistake.
///
/// `owned_by_org` decides whether `dismissal_restriction`, which personal repositories lack, is
/// a setting, and `public` whether `fork_pr_contributor_approval`, which private repositories
/// lack, is.
pub fn find(config: &Config, owned_by_org: bool, public: bool) -> (Vec<String>, Vec<Diagnostic>) {
    let unmanaged = &config.meta.unmanaged;
    let mut walker = Walker {
        unmanaged,
        used: vec![false; unmanaged.len()],
        out: Vec::new(),
    };

    let repository = serde_json::to_value(&config.repository).unwrap_or_default();
    walker.table("repository", &repository, Repository::KEYS, &[], &[]);

    let actions = serde_json::to_value(&config.actions).unwrap_or_default();
    let mut skip = Vec::new();
    // selected_actions has no effect unless allowed_actions is selected
    if config
        .actions
        .allowed_actions
        .as_deref()
        .is_some_and(|allowed| allowed != "selected")
    {
        skip.push("selected_actions");
    }
    if !public {
        skip.push("fork_pr_contributor_approval");
    }
    // include_claim_keys has no effect under the default template
    let custom = config
        .actions
        .oidc_subject_claim
        .as_ref()
        .is_some_and(|oidc| oidc.use_default == Some(false));
    if !custom {
        skip.push("oidc_subject_claim.include_claim_keys");
    }
    walker.table(
        "actions",
        &actions,
        Actions::KEYS,
        &[
            ("selected_actions", SelectedActions::KEYS),
            ("oidc_subject_claim", OidcSubjectClaim::KEYS),
        ],
        &skip,
    );

    for ruleset in &config.rulesets {
        let base = entry_path("ruleset", &ruleset.name);
        for (rule, value) in ruleset.rules.entries() {
            // A disabled rule has no parameters to keep
            if value == Json::Bool(false) {
                continue;
            }
            let keys = match rule.as_str() {
                "required_status_checks" => RequiredStatusChecks::KEYS,
                _ => match Rules::param_keys(rule_to_api(&rule)) {
                    Some(keys) => keys,
                    None => continue,
                },
            };
            let skip: &[&str] = match rule.as_str() {
                // A workflow file cannot be declared yet (see `WorkflowFile`)
                "workflows" => &["workflows"],
                "pull_request" if !owned_by_org => &["dismissal_restriction"],
                _ => &[],
            };
            walker.table(
                &format!("{base}.rules.{rule}"),
                &value,
                keys,
                &[("dismissal_restriction", DismissalRestriction::KEYS)],
                skip,
            );
        }
    }

    let errors = unmanaged
        .iter()
        .zip(&walker.used)
        .enumerate()
        .filter(|(_, (_, used))| !**used)
        .map(|(i, (entry, _))| {
            let path = format!("meta.unmanaged[{i}]");
            Diagnostic::error(
                Code::UnusedUnmanaged,
                format!("meta.unmanaged entry \"{entry}\" covers no unwritten setting"),
            )
            .at(&path)
            .help(
                "everything under it is written, or ghrs does not keep it unchanged when it is \
                unwritten; remove the entry",
            )
            .fix(Applicability::MachineApplicable, Edit::Remove { path })
        })
        .collect();
    (walker.out, errors)
}

/// Whether `--only` includes the resource `path` belongs to.
pub fn in_scope(path: &str, scope: &Scope) -> bool {
    match path.split('.').next() {
        Some("repository") => scope.repository,
        Some("actions") => scope.actions,
        Some("ruleset") => scope.rulesets,
        _ => true,
    }
}

/// The errors --strict raises for unwritten settings.
pub fn strict(paths: &[String]) -> Vec<Diagnostic> {
    paths
        .iter()
        .map(|path| {
            // A rule reported as a whole is enabled, with `true` or an empty table
            let rule = path
                .rsplit_once(".rules.")
                .is_some_and(|(_, rule)| !rule.contains('.'));
            let what = if rule {
                "write its parameters in a table (`true` keeps the current ones)"
            } else {
                "write it out"
            };
            Diagnostic::error(
                Code::UndeclaredSetting,
                format!("{path} is left unwritten, so plan keeps its current value"),
            )
            .at(path)
            .help(format!(
                "{what}, or list it in meta.unmanaged to leave it unchanged on purpose"
            ))
        })
        .collect()
}

struct Walker<'a> {
    unmanaged: &'a [String],
    /// Whether each entry of `unmanaged` covered an unwritten setting
    used: Vec<bool>,
    out: Vec<String>,
}

impl Walker<'_> {
    /// Reports the keys of the table at `path` that `declared` leaves out, or the table itself
    /// when it leaves out every key and `meta.unmanaged` covers none. Returns whether it did the
    /// latter. `nested` gives the keys of the tables inside, and `skip` the keys that are not
    /// settings here, with those of a table inside written as `table.key`.
    fn table(
        &mut self,
        path: &str,
        declared: &Json,
        keys: &[&str],
        nested: &[(&str, &[&str])],
        skip: &[&str],
    ) -> bool {
        let start = self.out.len();
        let mut whole = true;
        for key in keys.iter().filter(|key| !skip.contains(key)) {
            let at = format!("{path}.{key}");
            let value = declared.get(*key);
            if let Some((_, inner)) = nested.iter().find(|(k, _)| k == key) {
                let inner_skip: Vec<&str> = skip
                    .iter()
                    .filter_map(|s| s.strip_prefix(*key)?.strip_prefix('.'))
                    .collect();
                let all = self.table(&at, value.unwrap_or(&Json::Null), inner, &[], &inner_skip);
                whole &= value.is_none() && all;
            } else if value.is_some() || self.covered(&at) {
                whole = false;
            } else {
                self.out.push(at);
            }
        }
        let whole = whole && self.out.len() > start;
        if whole {
            self.out.truncate(start);
            self.out.push(path.to_string());
        }
        whole
    }

    /// Whether an entry of `meta.unmanaged` is `path` or a table containing it.
    fn covered(&mut self, path: &str) -> bool {
        let mut covered = false;
        for (entry, used) in self.unmanaged.iter().zip(&mut self.used) {
            let inside = path
                .strip_prefix(entry.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'));
            if inside {
                *used = true;
                covered = true;
            }
        }
        covered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;

    fn find_in(text: &str, owned_by_org: bool) -> (Vec<String>, Vec<String>) {
        let config = validate::parse(text).unwrap();
        let (paths, errors) = find(&config, owned_by_org, true);
        let errors = errors
            .iter()
            .map(|e| e.path.clone().unwrap_or_default())
            .collect();
        (paths, errors)
    }

    #[test]
    fn reports_unwritten_tables_as_a_whole() {
        let (paths, errors) = find_in("", false);
        assert_eq!(paths, ["repository", "actions"]);
        assert!(errors.is_empty());
    }

    #[test]
    fn reports_each_unwritten_key_of_a_written_table() {
        let text = "[repository]\ndescription = \"\"\nhomepage = \"\"\nvisibility = \"public\"\n\
            has_issues = true\nhas_wiki = false\nhas_projects = false\nhas_discussions = false\n\
            allow_merge_commit = true\nallow_squash_merge = true\nallow_rebase_merge = false\n\
            allow_auto_merge = true\ndefault_branch = \"main\"\ntopics = []\n\
            [actions]\nenabled = true\nallowed_actions = \"selected\"\n\
            [actions.selected_actions]\ngithub_owned_allowed = true\n";
        let (paths, _) = find_in(text, false);
        assert_eq!(
            paths,
            [
                "repository.delete_branch_on_merge",
                "actions.sha_pinning_required",
                "actions.default_workflow_permissions",
                "actions.can_approve_pull_request_reviews",
                "actions.fork_pr_contributor_approval",
                "actions.selected_actions.verified_allowed",
                "actions.selected_actions.patterns_allowed",
                "actions.oidc_subject_claim",
            ]
        );
    }

    #[test]
    fn selected_actions_counts_only_while_selected() {
        let text = "[actions]\nenabled = true\nallowed_actions = \"all\"\n\
            sha_pinning_required = false\ndefault_workflow_permissions = \"read\"\n\
            can_approve_pull_request_reviews = false\n\
            fork_pr_contributor_approval = \"first_time_contributors\"\n\
            [actions.oidc_subject_claim]\nuse_default = true\nuse_immutable_subject = true\n";
        let (paths, _) = find_in(text, false);
        assert_eq!(paths, ["repository"]);
    }

    #[test]
    fn skips_what_has_no_effect_on_the_repository() {
        let text = "[actions]\nenabled = true\nallowed_actions = \"all\"\n\
            sha_pinning_required = false\ndefault_workflow_permissions = \"read\"\n\
            can_approve_pull_request_reviews = false\n\
            [actions.oidc_subject_claim]\nuse_default = false\nuse_immutable_subject = true\n";
        let config = validate::parse(text).unwrap();
        // Private repositories have no fork PR approval
        let (paths, _) = find(&config, false, false);
        assert_eq!(
            paths,
            [
                "repository",
                "actions.oidc_subject_claim.include_claim_keys"
            ]
        );
    }

    #[test]
    fn reports_rule_parameters_kept_by_true_or_left_out() {
        let text = "[ruleset.main]\ntarget = \"branch\"\nenforcement = \"active\"\n\
            [ruleset.main.rules]\nrestrict_deletions = true\nrestrict_updates = true\n\
            copilot_code_review = false\nworkflows = true\n\
            [ruleset.main.rules.pull_request]\nallowed_merge_methods = [\"squash\"]\n\
            dismiss_stale_reviews_on_push = true\nrequire_code_owner_review = true\n\
            require_extra_approval_for_unattributed_changes = true\n\
            require_last_push_approval = true\nrequired_approving_review_count = 1\n\
            required_review_thread_resolution = true\n";
        let (paths, _) = find_in(text, false);
        assert_eq!(
            &paths[2..],
            [
                "ruleset.main.rules.pull_request.required_reviewers",
                "ruleset.main.rules.restrict_updates",
                "ruleset.main.rules.workflows",
            ]
        );

        let (paths, _) = find_in(text, true);
        assert!(paths.contains(&"ruleset.main.rules.pull_request.dismissal_restriction".into()));
    }

    #[test]
    fn unmanaged_covers_keys_and_tables() {
        let text = "[meta]\nunmanaged = [\"actions\", \"repository.homepage\"]\n\
            [repository]\ndescription = \"\"\n";
        let (paths, errors) = find_in(text, false);
        assert!(!paths.iter().any(|p| p.starts_with("actions")));
        assert!(!paths.contains(&"repository.homepage".into()));
        assert!(paths.contains(&"repository.visibility".into()));
        assert!(errors.is_empty());
    }

    #[test]
    fn unmanaged_entries_covering_nothing_are_errors() {
        let text = "[meta]\nunmanaged = [\"repository.description\", \"ruleset.gone\", \
            \"repository.has_wiki\", \"repository.has\"]\n\
            [repository]\ndescription = \"\"\n";
        let (_, errors) = find_in(text, false);
        assert_eq!(
            errors,
            [
                "meta.unmanaged[0]",
                "meta.unmanaged[1]",
                "meta.unmanaged[3]"
            ]
        );
    }
}
