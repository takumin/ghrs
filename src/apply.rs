//! `apply`. Executes the writes assembled by plan in order.
//!
//! No rollback on failure. Reports what was applied, failed, and skipped, and lets the next
//! `apply` reconverge.

use anyhow::Result;
use serde_json::{Value as Json, json};

use crate::access;
use crate::github::{GitHub, Method};
use crate::plan::Step;
use crate::repo::RepoRef;

pub enum Outcome {
    Applied,
    Failed(String),
    Skipped,
}

/// The result of each step, in execution order.
pub struct Report(pub Vec<(String, Outcome)>);

impl Report {
    pub fn count(&self, pred: fn(&Outcome) -> bool) -> usize {
        self.0.iter().filter(|(_, o)| pred(o)).count()
    }
}

/// With `fail_fast`, skips all remaining steps at the first failure. Otherwise, skips only the
/// steps that depend on a failed step.
pub async fn run(gh: &mut GitHub, target: &RepoRef, steps: &[Step], fail_fast: bool) -> Report {
    let mut report = Vec::new();
    let mut stop_all = false;
    let mut stop_rulesets = false;

    for step in steps {
        if stop_all || (stop_rulesets && step.is_ruleset()) {
            report.push((step.label(), Outcome::Skipped));
            continue;
        }
        match execute(gh, target, step).await {
            Ok(()) => report.push((step.label(), Outcome::Applied)),
            Err(err) => {
                report.push((step.label(), Outcome::Failed(format!("{err:#}"))));
                if fail_fast {
                    stop_all = true;
                } else if step.gates_rulesets() {
                    stop_rulesets = true;
                }
            }
        }
    }
    Report(report)
}

async fn execute(gh: &mut GitHub, target: &RepoRef, step: &Step) -> Result<()> {
    match step {
        Step::Repository { fields, topics } => {
            if !fields.is_empty() {
                let body = Json::Object(fields.clone());
                gh.send(Method::Patch, &format!("/repos/{target}"), Some(&body))
                    .await?;
            }
            if let Some(topics) = topics {
                gh.send(
                    Method::Put,
                    &format!("/repos/{target}/topics"),
                    Some(topics),
                )
                .await?;
            }
        }
        Step::Actions {
            permissions,
            selected_actions,
            workflow,
        } => {
            let writes = [
                ("", permissions),
                ("/selected-actions", selected_actions),
                ("/workflow", workflow),
            ];
            for (endpoint, body) in writes {
                if let Some(body) = body {
                    let route = format!("/repos/{target}/actions/permissions{endpoint}");
                    gh.send(Method::Put, &route, Some(body)).await?;
                }
            }
        }
        Step::GrantCollaborator { login, permission } => {
            let body = json!({ "permission": permission });
            gh.send(
                Method::Put,
                &format!("/repos/{target}/collaborators/{login}"),
                Some(&body),
            )
            .await?;
        }
        Step::UpdateInvitation { id, permission, .. } => {
            let body = json!({ "permissions": access::to_invitation(permission) });
            gh.send(
                Method::Patch,
                &format!("/repos/{target}/invitations/{id}"),
                Some(&body),
            )
            .await?;
        }
        Step::RevokeCollaborator { login } => {
            gh.send(
                Method::Delete,
                &format!("/repos/{target}/collaborators/{login}"),
                None,
            )
            .await?;
        }
        Step::DeleteInvitation { id, .. } => {
            gh.send(
                Method::Delete,
                &format!("/repos/{target}/invitations/{id}"),
                None,
            )
            .await?;
        }
        Step::GrantTeam { slug, permission } => {
            let body = json!({ "permission": permission });
            let org = &target.owner;
            gh.send(
                Method::Put,
                &format!("/orgs/{org}/teams/{slug}/repos/{target}"),
                Some(&body),
            )
            .await?;
        }
        Step::RevokeTeam { slug } => {
            let org = &target.owner;
            gh.send(
                Method::Delete,
                &format!("/orgs/{org}/teams/{slug}/repos/{target}"),
                None,
            )
            .await?;
        }
        Step::CreateRuleset { body, .. } => {
            gh.send(
                Method::Post,
                &format!("/repos/{target}/rulesets"),
                Some(body),
            )
            .await?;
        }
        Step::UpdateRuleset { id, body, .. } => {
            gh.send(
                Method::Put,
                &format!("/repos/{target}/rulesets/{id}"),
                Some(body),
            )
            .await?;
        }
        Step::DeleteRuleset { id, .. } => {
            gh.send(
                Method::Delete,
                &format!("/repos/{target}/rulesets/{id}"),
                None,
            )
            .await?;
        }
    }
    Ok(())
}
