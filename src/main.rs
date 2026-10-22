//! ghrs -- CLI that converges GitHub repository settings to the declaration file.

mod access;
mod apply;
mod auth;
mod cli;
mod diagnostic;
mod diff;
mod github;
mod import;
mod model;
mod plan;
mod render;
mod repo;
mod resolve;
mod schema;
mod state;
mod validate;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use apply::Outcome;
use cli::{ApplyArgs, AuthArgs, Cli, DriftArgs, ImportArgs, PlanArgs, Resource};
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
    let output = toml::to_string_pretty(&config)?;

    match &args.out {
        Some(path) => std::fs::write(path, output)
            .with_context(|| format!("failed to write {}", path.display()))?,
        None => out(&output)?,
    }

    for warning in &importer.warnings {
        note(&format!("warning: {warning}\n"));
    }
    note(&format!("API calls: {}\n", importer.gh.calls));

    Ok(ExitCode::SUCCESS)
}

async fn plan(args: PlanArgs, auth: &AuthArgs) -> Result<ExitCode> {
    let (gh, _, plan) = compute_plan(&args.config, args.repo, &args.only, args.prune, auth).await?;

    if args.json {
        out(&format!("{}\n", serde_json::to_string_pretty(&plan)?))?;
    } else {
        out(&render::text(&plan, args.verbose))?;
        print_writes(&gh, &plan).await?;
        for warning in &plan.warnings {
            note(&format!("warning: {warning}\n"));
        }
    }
    note(&format!("API calls: {}\n", gh.calls));

    Ok(ExitCode::SUCCESS)
}

/// Number of writes required by apply and the remaining rate limit. The remaining
/// count changes constantly, so it is not included in `--json` (determinism requirement).
async fn print_writes(gh: &GitHub, plan: &plan::Plan) -> Result<()> {
    if plan.writes == 0 {
        return Ok(());
    }
    out(&match gh.rate_limit().await {
        Ok((remaining, limit)) => format!(
            "writes needed for apply: {} (rate limit remaining: {remaining} / {limit})\n",
            plan.writes
        ),
        Err(_) => format!("writes needed for apply: {}\n", plan.writes),
    })
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
        compute_plan(&p.config, p.repo.clone(), &p.only, p.prune, auth).await?;

    // With --json, stdout carries only JSON. The plan display and confirmation prompt go to stderr
    if p.json {
        note(&render::text(&plan, p.verbose));
    } else {
        out(&render::text(&plan, p.verbose))?;
        print_writes(&gh, &plan).await?;
    }
    for warning in &plan.warnings {
        note(&format!("warning: {warning}\n"));
    }

    let confirmed = plan.steps.is_empty() || args.yes || confirm(&plan, &target)?;
    if !confirmed {
        note("apply cancelled\n");
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
    note(&format!("API calls: {}\n", gh.calls));

    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn drift(args: DriftArgs, auth: &AuthArgs) -> Result<ExitCode> {
    let (gh, _, plan) = compute_plan(&args.config, args.repo, &[], false, auth).await?;
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
    note(&format!("API calls: {}\n", gh.calls));

    Ok(if drifted && args.exit_code {
        ExitCode::from(2)
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
    auth: &AuthArgs,
) -> Result<(GitHub, RepoRef, plan::Plan)> {
    let config = load_config(path)?;
    let credential = auth::resolve(auth)?;
    let errors = validate::check(&config, credential.is_app());
    if !errors.is_empty() {
        return Err(in_file(Diagnostics::new(errors).into(), path));
    }
    let target = resolve_target(repo, &config)?;

    let (mut gh, self_app) = auth::connect(credential, &target).await?;
    let plan = plan::run(&mut gh, &config, &target, only, prune, self_app.as_ref())
        .await
        .map_err(|err| in_file(err, path))?;
    Ok((gh, target, plan))
}

/// Attach the declaration file path to validation errors.
fn in_file(err: anyhow::Error, path: &Path) -> anyhow::Error {
    match err.downcast::<Diagnostics>() {
        Ok(mut diagnostics) => {
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

fn load_config(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} not found; create it from the current settings with ghrs import --out {}",
            path.display(),
            path.display()
        ),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
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
