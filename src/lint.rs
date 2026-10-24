//! Validation of declaration files without the GitHub API, for `lint`. What plan reads from the
//! repository (whether the owner is an Organization, the current visibility) comes from flags or
//! the file instead, and the checks that need what neither gives are skipped and reported as such.

use serde::Serialize;

use crate::diagnostic::{Diagnostics, Severity};
use crate::plan::Scope;
use crate::{document, undeclared, validate};

/// What plan would read from the repository on GitHub. `None` is not known.
pub struct Context {
    pub owned_by_org: Option<bool>,
    /// The current visibility. A declared `repository.visibility` takes precedence, as in plan
    pub visibility: Option<&'static str>,
    pub strict: bool,
}

#[derive(Serialize)]
pub struct Report {
    #[serde(flatten)]
    pub diagnostics: Diagnostics,
    /// Checks skipped for lack of context, each with how to enable it
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<&'static str>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.diagnostics
            .diagnostics
            .iter()
            .any(|d| matches!(d.severity, Severity::Error))
    }
}

pub fn run(text: &str, context: &Context) -> Report {
    let mut report = check(text, context);
    document::locate(&mut report.diagnostics.diagnostics, text);
    report
}

/// The reports as GitHub Actions workflow commands, which annotate the lines of the files in pull
/// requests and in the run summary.
pub fn github(reports: &[Report]) -> String {
    let mut out = String::new();
    for report in reports {
        let file = report
            .diagnostics
            .file
            .as_deref()
            .map(|file| file.display().to_string());
        let file = file.as_deref().unwrap_or_default();
        for d in &report.diagnostics.diagnostics {
            let mut properties = format!("file={}", property(file));
            if let Some(location) = d.location {
                properties.push_str(&format!(",line={},col={}", location.line, location.column));
            }
            properties.push_str(&format!(",title={}", property(d.code.as_str())));
            let mut message = d.message.clone();
            if let Some(path) = &d.path {
                message.push_str(&format!("\n--> {path}"));
            }
            if let Some(help) = &d.help {
                message.push_str(&format!("\nhelp: {help}"));
            }
            out.push_str(&format!(
                "::{} {properties}::{}\n",
                d.severity(),
                data(&message)
            ));
        }
        for skipped in &report.skipped {
            out.push_str(&format!(
                "::notice file={},title=skipped::{}\n",
                property(file),
                data(&format!("skipped {skipped}"))
            ));
        }
    }
    out
}

/// Escape the message of a workflow command.
fn data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a property value of a workflow command.
fn property(s: &str) -> String {
    data(s).replace(':', "%3A").replace(',', "%2C")
}

fn check(text: &str, context: &Context) -> Report {
    let config = match validate::parse(text) {
        Ok(config) => config,
        Err(diagnostics) => {
            return Report {
                diagnostics,
                skipped: Vec::new(),
            };
        }
    };
    // The credential is not known offline, so `self` is accepted as App authentication would
    let mut diagnostics = validate::check(&config, true);

    let mut skipped = Vec::new();
    let owner = config.meta.owner.as_deref();
    if owner.is_none() && !config.collaborators.is_empty() {
        skipped.push("checks comparing collaborators with the owner; write meta.owner");
    }
    if context.owned_by_org.is_none() {
        skipped.push("checks for personal repositories; pass --owner-type");
    }
    let visibility = config
        .repository
        .visibility
        .as_deref()
        .or(context.visibility);
    if visibility.is_none() {
        skipped.push(
            "checks for private repositories; pass --visibility, or write repository.visibility",
        );
    }

    // What is not known is assumed to reject the least: an Organization-owned public repository
    let scope = Scope::new(&[]);
    diagnostics.extend(validate::check_context(
        &config,
        owner,
        context.owned_by_org.unwrap_or(true),
        visibility.unwrap_or("public"),
        &scope,
    ));

    // A setting is reported as unwritten only when it is a setting under every assumption, and an
    // entry of meta.unmanaged as unused only when it covers nothing under every assumption. The
    // fewest settings exist on a personal private repository, and the most on an Organization-owned
    // public one
    let public = visibility.map(|v| v == "public");
    let (paths, _) = undeclared::find(
        &config,
        context.owned_by_org.unwrap_or(false),
        public.unwrap_or(false),
    );
    let (_, unused) = undeclared::find(
        &config,
        context.owned_by_org.unwrap_or(true),
        public.unwrap_or(true),
    );
    diagnostics.extend(unused);
    diagnostics.extend(if context.strict {
        undeclared::strict(&paths)
    } else {
        undeclared::warnings(&paths)
    });

    Report {
        diagnostics: Diagnostics::new(diagnostics),
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Code;

    const UNKNOWN: Context = Context {
        owned_by_org: None,
        visibility: None,
        strict: false,
    };

    fn codes(text: &str, context: &Context) -> Vec<Code> {
        run(text, context)
            .diagnostics
            .diagnostics
            .iter()
            .map(|d| d.code)
            .collect()
    }

    fn ruleset(body: &str) -> String {
        format!("[ruleset.r]\ntarget = \"branch\"\nenforcement = \"active\"\n{body}")
    }

    #[test]
    fn reports_parse_and_check_errors() {
        assert_eq!(
            codes("[repository]\nhas_issue = true", &UNKNOWN),
            [Code::UnknownKey]
        );
        assert!(
            codes("[repository]\nvisibility = \"internal\"", &UNKNOWN)
                .contains(&Code::VisibilityNotAllowed)
        );
        // The credential is unknown offline, so `self` is not rejected
        let text = ruleset("bypass_actors = [{ type = \"integration\", name = \"self\" }]");
        assert!(!codes(&text, &UNKNOWN).contains(&Code::SelfNeedsApp));
    }

    #[test]
    fn checks_for_personal_repositories_need_the_owner_type() {
        let text = "[team.maintainers]\npermission = \"maintain\"";
        assert!(!codes(text, &UNKNOWN).contains(&Code::TeamOnPersonal));
        let user = Context {
            owned_by_org: Some(false),
            ..UNKNOWN
        };
        assert!(codes(text, &user).contains(&Code::TeamOnPersonal));
        assert!(
            run(text, &UNKNOWN)
                .skipped
                .iter()
                .any(|s| s.contains("--owner-type"))
        );
        assert!(
            run(text, &user)
                .skipped
                .iter()
                .all(|s| !s.contains("--owner-type"))
        );
    }

    #[test]
    fn declared_visibility_takes_precedence() {
        let text = format!(
            "[repository]\nvisibility = \"private\"\n{}",
            ruleset("[ruleset.r.rules]\nrestrict_deletions = true")
        );
        let public = Context {
            visibility: Some("public"),
            ..UNKNOWN
        };
        assert!(codes(&text, &public).contains(&Code::RulesetsOnPrivate));
        assert!(
            run(&text, &UNKNOWN)
                .skipped
                .iter()
                .all(|s| !s.contains("--visibility"))
        );

        let text = "[actions]\nfork_pr_contributor_approval = \"first_time_contributors\"";
        assert!(!codes(text, &UNKNOWN).contains(&Code::ForkPrApprovalOnPrivate));
        let private = Context {
            visibility: Some("private"),
            ..UNKNOWN
        };
        assert!(codes(text, &private).contains(&Code::ForkPrApprovalOnPrivate));
    }

    #[test]
    fn reports_unwritten_settings_as_warnings() {
        let text = "[repository]\nhas_wiki = false";
        let report = run(text, &UNKNOWN);
        assert!(!report.failed());
        assert!(
            report
                .diagnostics
                .diagnostics
                .iter()
                .all(|d| d.code == Code::UnwrittenSetting)
        );
        assert!(!report.diagnostics.diagnostics.is_empty());

        let strict = Context {
            strict: true,
            ..UNKNOWN
        };
        let report = run(text, &strict);
        assert!(report.failed());
        assert!(
            report
                .diagnostics
                .diagnostics
                .iter()
                .all(|d| d.code == Code::UndeclaredSetting)
        );
    }

    #[test]
    fn unknown_context_reports_what_holds_under_every_assumption() {
        // fork_pr_contributor_approval is a setting only on public repositories: unwritten, it is
        // not reported, and listed in meta.unmanaged, it is not reported as unused either
        let text = "[actions]\nenabled = true";
        let report = run(text, &UNKNOWN);
        assert!(
            report
                .diagnostics
                .diagnostics
                .iter()
                .all(|d| d.path.as_deref() != Some("actions.fork_pr_contributor_approval"))
        );
        let text = "[meta]\nunmanaged = [\"actions.fork_pr_contributor_approval\"]\n\
            [actions]\nenabled = true";
        assert!(!codes(text, &UNKNOWN).contains(&Code::UnusedUnmanaged));
        let private = Context {
            visibility: Some("private"),
            ..UNKNOWN
        };
        assert!(codes(text, &private).contains(&Code::UnusedUnmanaged));
    }

    #[test]
    fn reports_locations() {
        let text = "[repository]\nhas_wiki = false\nvisibility = \"internal\"\n";
        let report = run(text, &UNKNOWN);
        let d = report
            .diagnostics
            .diagnostics
            .iter()
            .find(|d| d.code == Code::VisibilityNotAllowed)
            .unwrap();
        assert_eq!(d.location.map(|l| (l.line, l.column)), Some((3, 1)));
    }

    #[test]
    fn writes_workflow_commands() {
        let mut report = run("[repository]\nhas_issue = true\n", &UNKNOWN);
        report.diagnostics.file = Some("dir,a:b/.ghrs.toml".into());
        let out = github(&[report]);
        let mut lines = out.lines();
        assert_eq!(
            lines.next(),
            Some(
                "::error file=dir%2Ca%3Ab/.ghrs.toml,line=2,col=1,title=E0002::unknown key \
                `has_issue`%0A--> repository.has_issue%0Ahelp: did you mean `has_issues`?"
            )
        );
        assert!(
            lines.all(
                |line| line.starts_with("::notice file=dir%2Ca%3Ab/.ghrs.toml,title=skipped::")
            )
        );
    }
}
