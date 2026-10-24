use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use std::path::PathBuf;

use crate::repo::RepoRef;

#[derive(Parser)]
#[command(
    version,
    about = "Converge GitHub repository settings to a declaration file"
)]
pub struct Cli {
    #[command(flatten)]
    pub auth: AuthArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Credentials. When omitted, resolved in the order GHRS_APP_ID ->
/// $XDG_CONFIG_HOME/ghrs/config.toml (~/.config/ghrs/config.toml by default) -> GITHUB_TOKEN ->
/// gh auth token. Private keys and tokens
/// also accept a 1Password secret reference (op://vault/item/field), read with `op read`
#[derive(Args)]
pub struct AuthArgs {
    /// App ID of the GitHub App
    #[arg(
        long,
        global = true,
        value_name = "ID",
        requires = "private_key",
        conflicts_with = "token"
    )]
    pub app_id: Option<u64>,

    /// Private key of the GitHub App (PEM file or op:// reference)
    #[arg(long, global = true, value_name = "PATH", requires = "app_id")]
    pub private_key: Option<PathBuf>,

    /// Personal access token (or op:// reference)
    #[arg(long, global = true, value_name = "TOKEN")]
    pub token: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show the difference between the declaration file and the current state
    Plan(PlanArgs),
    /// Apply the difference to GitHub
    Apply(ApplyArgs),
    /// Export the current settings in .ghrs.toml format
    Import(ImportArgs),
    /// Report only whether there is any difference
    Drift(DriftArgs),
    /// Validate declaration files without calling the GitHub API
    Lint(LintArgs),
    /// Write the JSON Schema: key meanings, types, and allowed values
    Schema(SchemaArgs),
    /// Show what keys of the declaration file mean, and their names in the GitHub API
    Explain(ExplainArgs),
    /// Write a shell completion script to standard output
    Completion(CompletionArgs),
}

impl Command {
    /// Whether `--json` is given. Decides whether validation errors are also output as JSON.
    pub fn json(&self) -> bool {
        match self {
            Command::Plan(args) => args.json,
            Command::Apply(args) => args.plan.json,
            Command::Drift(args) => args.json,
            Command::Lint(args) => args.json || args.format == Format::Json,
            Command::Import(_)
            | Command::Schema(_)
            | Command::Explain(_)
            | Command::Completion(_) => false,
        }
    }
}

#[derive(Args)]
pub struct PlanArgs {
    /// Target repository (taken from `[meta]`, then from git remote, if omitted)
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<RepoRef>,

    /// Path to the declaration file
    #[arg(long, value_name = "PATH", default_value = ".ghrs.toml")]
    pub config: PathBuf,

    /// Resource types to include
    #[arg(long, value_name = "RESOURCE,...", value_delimiter = ',')]
    pub only: Vec<Resource>,

    /// Delete resources that are not declared
    #[arg(long)]
    pub prune: bool,

    /// Fail when a setting ghrs knows is neither written nor listed in meta.unmanaged
    #[arg(long)]
    pub strict: bool,

    /// Also show unchanged fields
    #[arg(long)]
    pub verbose: bool,

    /// Output JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ApplyArgs {
    #[command(flatten)]
    pub plan: PlanArgs,

    /// Skip the confirmation prompt
    #[arg(long)]
    pub yes: bool,

    /// Stop at the first error (default)
    #[arg(long, overrides_with = "no_fail_fast")]
    pub fail_fast: bool,

    /// Continue with independent steps after an error
    #[arg(long, overrides_with = "fail_fast")]
    pub no_fail_fast: bool,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Target repository (detected from git remote if omitted)
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<RepoRef>,

    /// Output path (default: standard output)
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,

    /// JSON Schema (path or URL) the `#:schema` comment of the output references (default: the
    /// schema of this version on GitHub)
    #[arg(long, value_name = "PATH")]
    pub schema: Option<String>,

    /// Write the output without the comment header that references the JSON Schema
    #[arg(long, conflicts_with = "schema")]
    pub no_schema: bool,
}

#[derive(Args)]
pub struct ExplainArgs {
    /// Key path such as `ruleset.main.rules.pull_request` (lists every key if omitted)
    #[arg(value_name = "KEY")]
    pub key: Option<String>,

    /// Explain each key written in this declaration file instead
    #[arg(long, value_name = "PATH", conflicts_with = "key")]
    pub config: Option<PathBuf>,
}

#[derive(Args)]
pub struct SchemaArgs {
    /// Output path (default: standard output)
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

#[derive(Args)]
pub struct DriftArgs {
    /// Target repository (taken from `[meta]`, then from git remote, if omitted)
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<RepoRef>,

    /// Path to the declaration file
    #[arg(long, value_name = "PATH", default_value = ".ghrs.toml")]
    pub config: PathBuf,

    /// Output JSON
    #[arg(long)]
    pub json: bool,

    /// Exit with status 2 when there is a difference
    #[arg(long)]
    pub exit_code: bool,

    /// Fail when a setting ghrs knows is neither written nor listed in meta.unmanaged
    #[arg(long)]
    pub strict: bool,
}

#[derive(Args)]
pub struct LintArgs {
    /// Declaration files to validate
    #[arg(value_name = "PATH", default_value = ".ghrs.toml")]
    pub paths: Vec<PathBuf>,

    /// Who owns the repositories, for the checks that depend on it (skipped if omitted)
    #[arg(long, value_name = "TYPE")]
    pub owner_type: Option<OwnerType>,

    /// Current visibility of the repositories (repository.visibility takes precedence; the checks
    /// that depend on it are skipped if neither is given)
    #[arg(long, value_name = "VISIBILITY")]
    pub visibility: Option<Visibility>,

    /// Fail when a setting ghrs knows is neither written nor listed in meta.unmanaged
    #[arg(long)]
    pub strict: bool,

    /// Lints not to report, by code or group (overrides meta.lint)
    #[arg(short = 'A', long, value_name = "LINT,...", value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Lints to report as warnings, by code or group (overrides meta.lint)
    #[arg(short = 'W', long, value_name = "LINT,...", value_delimiter = ',')]
    pub warn: Vec<String>,

    /// Lints to report as errors, by code or group (overrides meta.lint); `warnings` makes every
    /// warning an error
    #[arg(short = 'D', long, value_name = "LINT,...", value_delimiter = ',')]
    pub deny: Vec<String>,

    /// Apply the suggested fixes that keep the meaning of the declaration, and write the file
    #[arg(long)]
    pub fix: bool,

    /// With --fix, also apply the suggested fixes that may not match the intent
    #[arg(long, requires = "fix")]
    pub unsafe_fixes: bool,

    /// Output format
    #[arg(
        long,
        value_name = "FORMAT",
        default_value = "text",
        conflicts_with = "json"
    )]
    pub format: Format,

    /// Output JSON (same as --format json)
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Human-readable, to standard error
    Text,
    /// JSON, to standard output
    Json,
    /// GitHub Actions workflow commands, which annotate the file in pull requests
    Github,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OwnerType {
    /// An Organization
    Org,
    /// A personal account
    User,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Visibility {
    Public,
    Private,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }
}

#[derive(Args)]
pub struct CompletionArgs {
    pub shell: Shell,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Resource {
    Repository,
    Actions,
    Ruleset,
    Collaborator,
    Team,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn only_accepts_comma_separated_list() {
        let cli = Cli::try_parse_from(["ghrs", "plan", "--only", "ruleset,team"]).unwrap();
        let Command::Plan(args) = cli.command else {
            panic!("not parsed as plan");
        };
        assert_eq!(args.only.len(), 2);
    }

    #[test]
    fn fail_fast_flags_override_each_other() {
        let cli = Cli::try_parse_from(["ghrs", "apply", "--no-fail-fast", "--fail-fast"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("not parsed as apply");
        };
        assert!(args.fail_fast);
        assert!(!args.no_fail_fast);
    }

    #[test]
    fn app_credentials_come_in_pairs() {
        assert!(Cli::try_parse_from(["ghrs", "plan", "--app-id", "1"]).is_err());
        assert!(Cli::try_parse_from(["ghrs", "plan", "--private-key", "k.pem"]).is_err());
        assert!(
            Cli::try_parse_from([
                "ghrs",
                "plan",
                "--app-id",
                "1",
                "--private-key",
                "k",
                "--token",
                "t"
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["ghrs", "plan", "--app-id", "1", "--private-key", "k.pem"])
            .unwrap();
        assert_eq!(cli.auth.app_id, Some(1));
    }
}
