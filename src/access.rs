//! Repository permissions for collaborators and teams.
//!
//! Both are a "unique key -> permission" mapping, so the diff computation is shared.

use anyhow::{Result, bail};
use serde_json::{Value as Json, json};

use crate::diff::{Action, FieldChange, Op, ResourceChange};
use crate::github::GitHub;
use crate::plan::Step;
use crate::repo::RepoRef;

/// A currently granted permission.
pub struct Grant {
    pub key: String,
    /// In `.ghrs.toml` vocabulary (pull / triage / push / maintain / admin)
    pub permission: String,
    /// The invitation ID, if the collaborator has not accepted the invitation
    pub invitation: Option<u64>,
}

/// Fetches collaborators and pending invitations, excluding the repository owner.
///
/// Without invitations, an invited but not-yet-accepted collaborator would be judged an "add"
/// every time, and apply would not be idempotent.
pub async fn current_collaborators(gh: &mut GitHub, target: &RepoRef) -> Result<Vec<Grant>> {
    let mut out = Vec::new();

    let list = gh
        .get_all(&format!("/repos/{target}/collaborators?affiliation=direct"))
        .await?;
    for c in &list {
        let Some(login) = c["login"].as_str() else {
            bail!("collaborator list entry has no login");
        };
        // In personal repositories, the owner is returned as admin (observed)
        if login.eq_ignore_ascii_case(&target.owner) {
            continue;
        }
        out.push(Grant {
            key: login.to_string(),
            permission: from_api(c["role_name"].as_str().unwrap_or_default()),
            invitation: None,
        });
    }

    let invitations = gh.get_all(&format!("/repos/{target}/invitations")).await?;
    for i in &invitations {
        let (Some(id), Some(login)) = (i["id"].as_u64(), i["invitee"]["login"].as_str()) else {
            continue;
        };
        out.push(Grant {
            key: login.to_string(),
            permission: from_api(i["permissions"].as_str().unwrap_or_default()),
            invitation: Some(id),
        });
    }
    Ok(out)
}

pub async fn current_teams(gh: &mut GitHub, target: &RepoRef) -> Result<Vec<Grant>> {
    let list = gh.get_all(&format!("/repos/{target}/teams")).await?;
    let mut out = Vec::new();
    for t in &list {
        let Some(slug) = t["slug"].as_str() else {
            bail!("team list entry has no slug");
        };
        out.push(Grant {
            key: slug.to_string(),
            permission: from_api(t["permission"].as_str().unwrap_or_default()),
            invitation: None,
        });
    }
    Ok(out)
}

/// `role_name` and invitation `permissions` use read / write. Normalize to the declaration's
/// vocabulary.
fn from_api(permission: &str) -> String {
    match permission {
        "read" => "pull",
        "write" => "push",
        other => other,
    }
    .to_string()
}

/// The invitation update API accepts the read / write vocabulary.
pub fn to_invitation(permission: &str) -> &str {
    match permission {
        "pull" => "read",
        "push" => "write",
        other => other,
    }
}

pub struct Diff {
    pub changes: Vec<ResourceChange>,
    pub steps: Vec<Step>,
    /// Not declared, but kept because `--prune` is not given
    pub ignored: Vec<String>,
    pub warnings: Vec<String>,
}

/// Compares the declaration (key, permission) with the current grants. Keys are
/// case-insensitive (as GitHub logins are).
pub fn diff(kind: &'static str, declared: &[(&str, &str)], current: &[Grant], prune: bool) -> Diff {
    let mut out = Diff {
        changes: Vec::new(),
        steps: Vec::new(),
        ignored: Vec::new(),
        warnings: Vec::new(),
    };
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    for (key, permission) in declared {
        let have = current.iter().find(|g| same(&g.key, key));
        let (action, field) = match have {
            None => (Action::Create, field(Op::Add, None, permission)),
            Some(g) if g.permission == *permission => {
                (Action::NoChange, field(Op::Same, None, permission))
            }
            Some(g) => (
                Action::Update,
                field(Op::Change, Some(&g.permission), permission),
            ),
        };
        if let Some(g) = have
            && g.invitation.is_some()
        {
            out.warnings
                .push(format!("{kind} \"{key}\" has not accepted the invitation"));
        }
        if action != Action::NoChange {
            out.steps.push(grant_step(kind, key, permission, have));
        }
        out.changes.push(ResourceChange {
            kind,
            name: Some(key.to_string()),
            action,
            fields: vec![field],
            destructive: Vec::new(),
        });
    }

    for g in current {
        if declared.iter().any(|(key, _)| same(key, &g.key)) {
            continue;
        }
        if !prune {
            out.ignored.push(format!("{kind} \"{}\"", g.key));
            continue;
        }
        out.steps.push(revoke_step(kind, g));
        out.changes.push(ResourceChange {
            kind,
            name: Some(g.key.clone()),
            action: Action::Delete,
            fields: vec![FieldChange {
                op: Op::Remove,
                path: "permission".to_string(),
                before: Some(json!(g.permission)),
                after: None,
            }],
            destructive: Vec::new(),
        });
    }
    out
}

fn field(op: Op, before: Option<&str>, after: &str) -> FieldChange {
    FieldChange {
        op,
        path: "permission".to_string(),
        before: before.map(|b| json!(b)),
        after: Some(Json::String(after.to_string())),
    }
}

fn grant_step(kind: &str, key: &str, permission: &str, have: Option<&Grant>) -> Step {
    let key = key.to_string();
    let permission = permission.to_string();
    match (kind, have.and_then(|g| g.invitation)) {
        ("team", _) => Step::GrantTeam {
            slug: key,
            permission,
        },
        (_, Some(id)) => Step::UpdateInvitation {
            login: key,
            id,
            permission,
        },
        _ => Step::GrantCollaborator {
            login: key,
            permission,
        },
    }
}

fn revoke_step(kind: &str, g: &Grant) -> Step {
    let key = g.key.clone();
    match (kind, g.invitation) {
        ("team", _) => Step::RevokeTeam { slug: key },
        (_, Some(id)) => Step::DeleteInvitation { login: key, id },
        _ => Step::RevokeCollaborator { login: key },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(key: &str, permission: &str, invitation: Option<u64>) -> Grant {
        Grant {
            key: key.to_string(),
            permission: permission.to_string(),
            invitation,
        }
    }

    fn actions(d: &Diff) -> Vec<(Action, &str)> {
        d.changes
            .iter()
            .map(|c| (c.action, c.name.as_deref().unwrap_or_default()))
            .collect()
    }

    #[test]
    fn compares_permissions_by_key() {
        let current = [
            grant("Alice", "push", None),
            grant("bob", "pull", None),
            grant("carol", "push", None),
        ];
        let d = diff(
            "collaborator",
            &[("alice", "push"), ("bob", "maintain"), ("dave", "pull")],
            &current,
            false,
        );
        assert_eq!(
            actions(&d),
            [
                (Action::NoChange, "alice"),
                (Action::Update, "bob"),
                (Action::Create, "dave"),
            ]
        );
        assert_eq!(d.ignored, ["collaborator \"carol\""]);
        assert!(matches!(
            &d.steps[..],
            [
                Step::GrantCollaborator { .. },
                Step::GrantCollaborator { .. }
            ]
        ));

        let d = diff("collaborator", &[], &current, true);
        assert_eq!(d.changes.len(), 3);
        assert!(
            d.steps
                .iter()
                .all(|s| matches!(s, Step::RevokeCollaborator { .. }))
        );
    }

    /// An invited but not-yet-accepted collaborator results in updating or revoking the
    /// invitation, not an add
    #[test]
    fn pending_invitations_count_as_current() {
        let current = [grant("alice", "push", Some(7))];
        let d = diff("collaborator", &[("alice", "push")], &current, false);
        assert_eq!(actions(&d), [(Action::NoChange, "alice")]);
        assert_eq!(d.warnings.len(), 1);

        let d = diff("collaborator", &[("alice", "pull")], &current, false);
        assert!(matches!(
            &d.steps[..],
            [Step::UpdateInvitation { id: 7, .. }]
        ));

        let d = diff("collaborator", &[], &current, true);
        assert!(matches!(
            &d.steps[..],
            [Step::DeleteInvitation { id: 7, .. }]
        ));
    }

    #[test]
    fn teams_use_team_steps() {
        let d = diff(
            "team",
            &[("maintainers", "maintain")],
            &[grant("old", "push", None)],
            true,
        );
        assert!(matches!(
            &d.steps[..],
            [Step::GrantTeam { .. }, Step::RevokeTeam { .. }]
        ));
    }

    #[test]
    fn normalizes_permission_names() {
        assert_eq!(from_api("write"), "push");
        assert_eq!(from_api("read"), "pull");
        assert_eq!(from_api("maintain"), "maintain");
        assert_eq!(to_invitation("push"), "write");
        assert_eq!(to_invitation("admin"), "admin");
    }
}
