//! ghrs -- CLI that converges GitHub repository settings to the declaration file.

mod access;
mod actions;
mod apply;
mod auth;
mod cli;
mod diagnostic;
mod diff;
mod document;
mod explain;
mod github;
mod import;
mod lint;
mod lints;
mod model;
mod plan;
mod render;
mod repo;
mod resolve;
mod schema;
mod state;
mod undeclared;
mod validate;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use apply::Outcome;
use cli::{ApplyArgs, AuthArgs, Cli, DriftArgs, ImportArgs, LintArgs, PlanArgs, Resource};
use diagnostic::Diagnostics;
use github::GitHub;
use model::Config;
use repo::RepoRef;

#[tokio::main]
async fn main() -> ExitCode {
    // clap returns exit 2 on argument errors, but ghrs reserves 2 for "drift found a diff".
    // Map argument errors to 1 as well.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let json = cli.command.json();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            match err.downcast_ref::<Diagnostics>() {
                Some(diagnostics) if json => match serde_json::to_string_pretty(diagnostics) {
                    Ok(text) => {
                        let _ = out(&format!("{text}\n"));
                    }
                    Err(_) => note(&format!("{diagnostics}\n")),
                },
                Some(diagnostics) => note(&format!("{diagnostics}\n")),
                None => note(&format!("error: {err:#}\n")),
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        cli::Command::Import(args) => import(args, &cli.auth).await,
        cli::Command::Plan(args) => plan(args, &cli.auth).await,
        cli::Command::Apply(args) => apply(args, &cli.auth).await,
        cli::Command::Drift(args) => drift(args, &cli.auth).await,
        cli::Command::Lint(args) => lint(args),
        cli::Command::Schema(args) => {
            let text = format!("{}\n", serde_json::to_string_pretty(&model::json_schema())?);
            write_output(args.out.as_deref(), &text)?;
            Ok(ExitCode::SUCCESS)
        }
        cli::Command::Explain(args) => {
            let text = match (&args.key, &args.config) {
                (_, Some(path)) => {
                    let text = std::fs::read_to_string(path)
                        .with_context(|| format!("cannot read {}", path.display()))?;
                    explain::config(&text).with_context(|| path.display().to_string())?
                }
                (Some(key), None) => explain::key(key)?,
                (None, None) => {
                    out(&explain::index())?;
                    note(explain::HINT);
                    return Ok(ExitCode::SUCCESS);
                }
            };
            out(&text)?;
            Ok(ExitCode::SUCCESS)
        }
        cli::Command::Completion(args) => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            let mut script = Vec::new();
            clap_complete::generate(args.shell, &mut command, name, &mut script);
            out(&String::from_utf8_lossy(&script))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn import(args: ImportArgs, auth: &AuthArgs) -> Result<ExitCode> {
    let target = repo::resolve(args.repo)?;

    let (gh, self_app) = auth::connect(auth::resolve(auth)?, &target).await?;
    let mut importer = import::Importer::new(gh, self_app.as_ref());
    let config = importer.run(&target.owner, &target.name).await?;
    let mut output = import::render(&config)?;
    if !args.no_schema {
        output = format!("{}\n{output}", schema_header(args.schema.as_deref()));
    }
    write_output(args.out.as_deref(), &output)?;

    for warning in &importer.warnings {
        note(&format!("warning: {warning}\n"));
    }
    note_calls(&importer.gh).await;

    Ok(ExitCode::SUCCESS)
}

/// Schema committed in this repository at the tag of this version
const SCHEMA_URL: &str = concat!(
    "https://raw.githubusercontent.com/takumin/ghrs/v",
    env!("CARGO_PKG_VERSION"),
    "/schema/ghrs.schema.json"
);

/// Comment header of the import output. taplo reads the schema from the directive on the first
/// line, and the second line keeps readers (coding agents above all) from guessing key meanings
/// from GitHub's API.
fn schema_header(schema: Option<&str>) -> String {
    const NOTE: &str =
        "# Take key meanings from `ghrs explain`, not from key names or the GitHub API.";
    format!("#:schema {}\n{NOTE}\n", schema.unwrap_or(SCHEMA_URL))
}

async fn plan(args: PlanArgs, auth: &AuthArgs) -> Result<ExitCode> {
    let (gh, _, plan) = compute_plan(
        &args.config,
        args.repo,
        &args.only,
        args.prune,
        args.strict,
        auth,
    )
    .await?;

    if args.json {
        out(&format!("{}\n", serde_json::to_string_pretty(&plan)?))?;
    } else {
        out(&render::text(&plan, args.verbose))?;
        print_writes(&plan)?;
        for warning in &plan.warnings {
            note(&format!("warning: {warning}\n"));
        }
    }
    note_calls(&gh).await;

    Ok(ExitCode::SUCCESS)
}

/// Number of writes required by apply.
fn print_writes(plan: &plan::Plan) -> Result<()> {
    if plan.writes == 0 {
        return Ok(());
    }
    out(&format!("writes needed for apply: {}\n", plan.writes))
}

/// Number of API calls made, with the rate limit left afterwards. The remaining count changes
/// constantly, so it goes to stderr and never into `--json` (determinism requirement).
async fn note_calls(gh: &GitHub) {
    note(&match gh.rate_limit().await {
        Ok((remaining, limit)) => format!(
            "API calls: {} (rate limit: {remaining} / {limit} remaining)\n",
            gh.calls
        ),
        Err(_) => format!("API calls: {}\n", gh.calls),
    });
}

/// Write to `path`, or to stdout without one.
fn write_output(path: Option<&Path>, text: &str) -> Result<()> {
    match path {
        Some(path) => std::fs::write(path, text)
            .with_context(|| format!("failed to write {}", path.display())),
        None => out(text),
    }
}

/// Write to stderr. Failures are ignored, since there is nowhere to report them.
/// `eprint!` panics when the reader closes with `2>&1 | head`.
fn note(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr
        .write_all(text.as_bytes())
        .and_then(|()| stderr.flush());
}

/// Write to stdout. If the reader closes first (e.g. `| head`), discard the rest and keep going
/// without changing the exit code. `print!` would panic, stopping `apply --yes | head` before
/// applying.
fn out(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result.context("failed to write to standard output"),
    }
}

async fn apply(args: ApplyArgs, auth: &AuthArgs) -> Result<ExitCode> {
    let p = &args.plan;
    let (mut gh, target, plan) =
        compute_plan(&p.config, p.repo.clone(), &p.only, p.prune, p.strict, auth).await?;

    // With --json, stdout carries only JSON. The plan display and confirmation prompt go to stderr
    if p.json {
        note(&render::text(&plan, p.verbose));
    } else {
        out(&render::text(&plan, p.verbose))?;
        print_writes(&plan)?;
    }
    for warning in &plan.warnings {
        note(&format!("warning: {warning}\n"));
    }

    let confirmed = plan.steps.is_empty() || args.yes || confirm(&plan, &target)?;
    if !confirmed {
        note("apply cancelled\n");
        if p.json {
            // Every step is reported as skipped, so the output still parses as an apply result
            let report = apply::Report(
                plan.steps
                    .iter()
                    .map(|s| (s.label(), Outcome::Skipped))
                    .collect(),
            );
            out(&format!(
                "{}\n",
                serde_json::to_string_pretty(&render::apply_json(&plan, &report))?
            ))?;
        }
        return Ok(ExitCode::FAILURE);
    }

    let fail_fast = args.fail_fast || !args.no_fail_fast;
    let report = apply::run(&mut gh, &target, &plan.steps, fail_fast).await;
    let failed = report.count(|o| matches!(o, Outcome::Failed(_)));

    if p.json {
        out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&render::apply_json(&plan, &report))?
        ))?;
    } else if !plan.steps.is_empty() {
        out(&render::apply_text(&report))?;
    }
    note_calls(&gh).await;

    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn drift(args: DriftArgs, auth: &AuthArgs) -> Result<ExitCode> {
    let (gh, _, plan) =
        compute_plan(&args.config, args.repo, &[], false, args.strict, auth).await?;
    let drifted = plan.summary.add + plan.summary.change + plan.summary.destroy > 0;

    if args.json {
        out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&render::drift_json(&plan))?
        ))?;
    } else {
        out(&render::drift_text(&plan))?;
        for warning in &plan.warnings {
            note(&format!("warning: {warning}\n"));
        }
    }
    note_calls(&gh).await;

    Ok(if drifted && args.exit_code {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

/// Validate each declaration file with what `plan` checks, taking what plan reads from GitHub
/// from the flags instead. Reads no credentials, so it runs anywhere the files are.
fn lint(args: LintArgs) -> Result<ExitCode> {
    let context = lint::Context {
        owned_by_org: args.owner_type.map(|t| matches!(t, cli::OwnerType::Org)),
        visibility: args.visibility.map(cli::Visibility::as_str),
        strict: args.strict,
        levels: model::LintLevels {
            allow: args.allow,
            warn: args.warn,
            deny: args.deny,
        },
    };
    if let Some(name) = [
        &context.levels.allow,
        &context.levels.warn,
        &context.levels.deny,
    ]
    .into_iter()
    .flatten()
    .find(|name| !lints::is_name(name))
    {
        bail!("\"{name}\" is not a lint; write a lint code such as W0301, or a group");
    }
    let format = if args.json {
        cli::Format::Json
    } else {
        args.format
    };
    let mut reports = Vec::new();
    for path in &args.paths {
        let mut text = read_config(path)?;
        if args.fix {
            let (fixed, applied) = lint::fix(&text, &context, args.unsafe_fixes);
            if applied > 0 {
                std::fs::write(path, &fixed)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                let plural = if applied == 1 { "" } else { "es" };
                note(&format!(
                    "{}: applied {applied} fix{plural}\n",
                    path.display()
                ));
                text = fixed;
            }
        }
        let mut report = lint::run(&text, &context);
        report.diagnostics.file = Some(path.clone());
        reports.push(report);
    }
    let failed = reports.iter().any(lint::Report::failed);

    match format {
        cli::Format::Json => out(&format!("{}\n", serde_json::to_string_pretty(&reports)?))?,
        cli::Format::Github => out(&lint::github(&reports))?,
        cli::Format::Text => {
            for report in &reports {
                if !report.diagnostics.diagnostics.is_empty() {
                    note(&format!("{}\n", report.diagnostics));
                }
                for skipped in &report.skipped {
                    let file = report.diagnostics.file.as_deref().unwrap_or(Path::new(""));
                    note(&format!("note: {}: skipped {skipped}\n", file.display()));
                }
            }
        }
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Common preamble for plan / apply / drift. apply recomputes the plan before applying.
async fn compute_plan(
    path: &Path,
    repo: Option<RepoRef>,
    only: &[Resource],
    prune: bool,
    strict: bool,
    auth: &AuthArgs,
) -> Result<(GitHub, RepoRef, plan::Plan)> {
    let text = read_config(path)?;
    let config = validate::parse(&text).map_err(|d| in_file(d.into(), path, &text))?;
    // Report declaration errors before reading credentials, which may run `op` or `gh`. Only
    // `self` depends on the credential, so it is checked once the credential is known
    let invalid = |is_app| {
        let errors = validate::check(&config, is_app);
        (!errors.is_empty()).then(|| in_file(Diagnostics::new(errors).into(), path, &text))
    };
    if let Some(err) = invalid(true) {
        return Err(err);
    }
    let credential = auth::resolve(auth)?;
    if let Some(err) = invalid(credential.is_app()) {
        return Err(err);
    }
    let target = resolve_target(repo, &config)?;

    let (mut gh, self_app) = auth::connect(credential, &target).await?;
    let plan = plan::run(
        &mut gh,
        &config,
        &target,
        only,
        prune,
        strict,
        self_app.as_ref(),
    )
    .await
    .map_err(|err| in_file(err, path, &text))?;
    Ok((gh, target, plan))
}

/// Attach the declaration file path, and where in `text` each error is, to validation errors.
fn in_file(err: anyhow::Error, path: &Path, text: &str) -> anyhow::Error {
    match err.downcast::<Diagnostics>() {
        Ok(mut diagnostics) => {
            document::locate(&mut diagnostics.diagnostics, text);
            diagnostics.file = Some(path.to_path_buf());
            diagnostics.into()
        }
        Err(err) => err,
    }
}

/// Confirmation before apply. When there is a protection-weakening change, require
/// typing the repository name instead of `yes`, as an additional confirmation.
fn confirm(plan: &plan::Plan, target: &RepoRef) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "cannot ask for confirmation because standard input is not a terminal; run with --yes"
        );
    }
    let destructive = plan.changes.iter().any(|c| !c.destructive.is_empty());
    let expected = if destructive {
        note(&format!(
            "\nThese changes weaken protection. Type the repository name {target} to apply: "
        ));
        target.to_string()
    } else {
        note("\nApply these changes? Type yes to proceed: ");
        "yes".to_string()
    };

    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim() == expected)
}

fn read_config(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} not found; create it from the current settings with ghrs import --out {}",
            path.display(),
            path.display()
        ),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Determine the target repository in the order `--repo` -> `[meta]` -> git remote.
fn resolve_target(flag: Option<RepoRef>, config: &Config) -> Result<RepoRef> {
    if flag.is_some() {
        return repo::resolve(flag);
    }
    match (&config.meta.owner, &config.meta.repo) {
        (Some(owner), Some(name)) => Ok(RepoRef {
            owner: owner.clone(),
            name: name.clone(),
        }),
        (None, None) => repo::resolve(None),
        _ => bail!("[meta] needs both owner and repo, or neither"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_header_references_the_schema_of_this_version() {
        let header = schema_header(None);
        let mut lines = header.lines();
        let url = format!(
            "https://raw.githubusercontent.com/takumin/ghrs/v{}/schema/ghrs.schema.json",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(lines.next(), Some(format!("#:schema {url}").as_str()));
        assert!(lines.all(|line| line.starts_with("# ")));
        assert!(header.ends_with('\n'));
        assert!(
            schema_header(Some("./ghrs.schema.json")).starts_with("#:schema ./ghrs.schema.json\n")
        );
    }
}
