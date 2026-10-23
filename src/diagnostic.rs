//! Diagnostics on the declaration file.
//!
//! The human-readable output and `--json` are built from the same data. Codes and suggested fixes
//! are kept as structured data so that coding agents can branch on the code rather than the
//! message text, and apply suggested fixes as-is.

use serde::Serialize;
use serde_json::Value as Json;
use std::fmt;
use std::path::PathBuf;

use crate::schema;

macro_rules! codes {
    ($($name:ident = $code:literal,)*) => {
        /// Diagnostic code. A number is never reused for a different meaning.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Code {
            $($name,)*
        }

        impl Code {
            #[cfg(test)]
            pub const ALL: &[Code] = &[$(Code::$name,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Code::$name => $code,)*
                }
            }
        }
    };
}

codes! {
    // E00xx: syntax and shape of the file
    InvalidToml = "E0001",
    UnknownKey = "E0002",
    InvalidValue = "E0003",
    // E01xx: repository-wide
    VisibilityNotAllowed = "E0101",
    TooManyRulesets = "E0102",
    RulesetsOnPrivate = "E0103",
    // E02xx: ruleset
    // E0201 (duplicate ruleset name) is retired: rulesets are keyed by name, so TOML itself
    // rejects a duplicate
    EmptyRulesetName = "E0202",
    TargetNotAllowed = "E0203",
    EvaluateEnforcement = "E0204",
    EnforcementNotAllowed = "E0205",
    RefPatternPrefix = "E0206",
    RuleTargetMismatch = "E0207",
    PushRule = "E0208",
    // E0209 (rule neither a boolean nor a table), E0211 (checks not an array) and E0212 (malformed
    // status check) are retired: rules are typed, so parsing reports them as E0003
    NumericIdKey = "E0210",
    OrgOnlyRule = "E0213",
    DismissalRestrictionOnPersonal = "E0214",
    // E03xx: bypass actors and pull_request actors
    UnknownActorType = "E0301",
    BypassModeNotAllowed = "E0302",
    PullRequestModeTarget = "E0303",
    DeployKeyPullRequestMode = "E0304",
    ActorTakesNoName = "E0305",
    ActorMissingName = "E0306",
    SelfNeedsApp = "E0307",
    NumericActorName = "E0308",
    RoleWithoutWrite = "E0309",
    UnknownRole = "E0310",
    TeamActorOnPersonal = "E0311",
    OrganizationAdminOnPersonal = "E0312",
    // E0313 (malformed pull_request actor) is retired: parsing reports it as E0002 / E0003
    ReviewActorTypeNotAllowed = "E0314",
    DismissalRoleNotAdmin = "E0315",
    // E04xx: collaborators and teams
    EmptyLogin = "E0401",
    DuplicateCollaborator = "E0402",
    DuplicateTeam = "E0403",
    PermissionNotAllowed = "E0404",
    OwnerAsCollaborator = "E0405",
    PersonalCollaboratorPermission = "E0406",
    TeamOnPersonal = "E0407",
}

impl Serialize for Code {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
}

/// Whether a suggested fix may be applied as-is. Modeled after rustc's Applicability.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Applicability {
    /// Fixes without changing the meaning of the declaration. May be applied without confirmation
    MachineApplicable,
    /// One possible fix. Confirm it matches the intent before applying
    MaybeIncorrect,
}

/// An edit to `.ghrs.toml`.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Edit {
    Remove {
        path: String,
    },
    Set {
        path: String,
        value: Json,
    },
    /// Renames the last key of `path` to `to`, keeping its value
    Rename {
        path: String,
        to: String,
    },
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Fix {
    pub applicability: Applicability,
    pub edit: Edit,
}

#[derive(Serialize, Debug)]
pub struct Diagnostic {
    pub code: Code,
    pub severity: Severity,
    pub message: String,
    /// Location in the declaration file (e.g. `ruleset.main.bypass_actors[1]`). Absent for
    /// file-wide diagnostics
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
    /// Where GitHub's REST API description defines what `path` may contain. Set for paths under
    /// a ruleset's `rules`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

impl Diagnostic {
    pub fn error(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
            path: None,
            help: None,
            fix: None,
            reference: None,
        }
    }

    pub fn at(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.reference = schema::rule_reference(&path);
        self.path = Some(path);
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn fix(mut self, applicability: Applicability, edit: Edit) -> Self {
        self.fix = Some(Fix {
            applicability,
            edit,
        });
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.code.as_str(), self.message)?;
        if let Some(path) = &self.path {
            write!(f, "\n  --> {path}")?;
        }
        if let Some(help) = &self.help {
            write!(f, "\n  = help: {help}")?;
        }
        if let Some(fix) = &self.fix {
            let applicability = match fix.applicability {
                Applicability::MachineApplicable => "machine-applicable",
                Applicability::MaybeIncorrect => "maybe-incorrect",
            };
            let edit = match &fix.edit {
                Edit::Remove { path } => format!("remove {path}"),
                Edit::Set { path, value } => format!("set {path} = {value}"),
                Edit::Rename { path, to } => format!("rename {path} to {to}"),
            };
            write!(f, "\n  = fix ({applicability}): {edit}")?;
        }
        if let Some(reference) = &self.reference {
            write!(f, "\n  = reference: {reference}")?;
        }
        Ok(())
    }
}

/// The diagnostics found by validation. Returned wrapped in `anyhow::Error`; with `--json`, the
/// caller extracts it and outputs it as structured data.
#[derive(Serialize)]
pub struct Diagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new(diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            file: None,
            diagnostics,
        }
    }
}

/// Like rustc, lists diagnostics separated by blank lines and summarizes the count at the end.
impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diagnostic in &self.diagnostics {
            write!(f, "{diagnostic}\n\n")?;
        }
        let n = self.diagnostics.len();
        let plural = if n == 1 { "" } else { "s" };
        write!(f, "error: aborting due to {n} error{plural}")?;
        if let Some(file) = &self.file {
            write!(f, " in {}", file.display())?;
        }
        Ok(())
    }
}

impl fmt::Debug for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for Diagnostics {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique() {
        let mut seen = HashSet::new();
        for code in Code::ALL {
            assert!(seen.insert(code.as_str()), "{code:?}");
        }
    }

    #[test]
    fn renders_text_and_json_from_the_same_data() {
        let d = Diagnostic::error(Code::OrganizationAdminOnPersonal, "m")
            .at("ruleset.main.bypass_actors[1]")
            .help("h")
            .fix(
                Applicability::MachineApplicable,
                Edit::Remove {
                    path: "ruleset.main.bypass_actors[1]".to_string(),
                },
            );
        assert_eq!(
            d.to_string(),
            "error[E0312]: m\n  --> ruleset.main.bypass_actors[1]\n  = help: h\n  \
            = fix (machine-applicable): remove ruleset.main.bypass_actors[1]"
        );
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({
                "code": "E0312",
                "severity": "error",
                "message": "m",
                "path": "ruleset.main.bypass_actors[1]",
                "help": "h",
                "fix": {
                    "applicability": "machine-applicable",
                    "edit": { "op": "remove", "path": "ruleset.main.bypass_actors[1]" }
                }
            })
        );
    }
}
