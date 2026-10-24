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
    /// Write the JSON Schema of the declaration file: each key's meaning, type, and allowed values
    Schema(SchemaArgs),
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
            Command::Import(_) | Command::Schema(_) | Command::Completion(_) => false,
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
}

#[derive(Args)]
pub struct CompletionArgs {
    pub shell: Shell,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Resource {
    Repository,
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
