use serde_json::{Value as Json, json};
use std::fmt::Write;

use crate::apply::{Outcome, Report};
use crate::diff::{Action, FieldChange, Op};
use crate::plan::Plan;

/// drift only reports presence per resource. Use plan for field-level details.
pub fn drift_text(plan: &Plan) -> String {
    let mut out = String::new();
    let drifted: Vec<_> = plan
        .changes
        .iter()
        .filter(|c| c.action != Action::NoChange)
        .collect();
    for change in &drifted {
        let _ = writeln!(out, "{} {}", symbol(change.action), title(change));
    }
    if drifted.is_empty() {
        out.push_str("No drift.\n");
    } else {
        let _ = writeln!(
            out,
            "\nDrift: {} resource(s) differ from {}. Run ghrs plan for details.",
            drifted.len(),
            plan.repository
        );
    }
    out
}

pub fn drift_json(plan: &Plan) -> Json {
    let resources: Vec<Json> = plan
        .changes
        .iter()
        .filter(|c| c.action != Action::NoChange)
        .map(|c| json!({ "kind": c.kind, "name": c.name, "action": c.action }))
        .collect();
    json!({
        "repository": plan.repository,
        "drift": !resources.is_empty(),
        "resources": resources,
        "warnings": plan.warnings,
    })
}

fn symbol(action: Action) -> &'static str {
    match action {
        Action::Create => "+",
        Action::Update => "~",
        Action::Delete => "-",
        Action::NoChange => " ",
    }
}

fn title(change: &crate::diff::ResourceChange) -> String {
    match &change.name {
        Some(name) => format!("{}[{}]", change.kind, quote(name)),
        None => change.kind.to_string(),
    }
}

pub fn apply_text(report: &Report) -> String {
    let mut out = String::from("\n");
    for (label, outcome) in &report.0 {
        let _ = match outcome {
            Outcome::Applied => writeln!(out, "applied: {label}"),
            Outcome::Failed(err) => {
                writeln!(out, "failed:  {label}\n  {}", err.replace('\n', "\n  "))
            }
            Outcome::Skipped => writeln!(out, "skipped: {label}"),
        };
    }
    let _ = writeln!(
        out,
        "\nApply: {} applied, {} failed, {} skipped.",
        report.count(|o| matches!(o, Outcome::Applied)),
        report.count(|o| matches!(o, Outcome::Failed(_))),
        report.count(|o| matches!(o, Outcome::Skipped)),
    );
    out
}

/// apply result for CI. Includes the plan as-is and lists each step's result in execution order.
pub fn apply_json(plan: &Plan, report: &Report) -> Json {
    let results: Vec<Json> = report
        .0
        .iter()
        .map(|(label, outcome)| match outcome {
            Outcome::Applied => json!({ "step": label, "outcome": "applied" }),
            Outcome::Failed(err) => json!({ "step": label, "outcome": "failed", "error": err }),
            Outcome::Skipped => json!({ "step": label, "outcome": "skipped" }),
        })
        .collect();
    json!({
        "plan": plan,
        "results": results,
        "summary": {
            "applied": report.count(|o| matches!(o, Outcome::Applied)),
            "failed": report.count(|o| matches!(o, Outcome::Failed(_))),
            "skipped": report.count(|o| matches!(o, Outcome::Skipped)),
        },
    })
}

pub fn text(plan: &Plan, verbose: bool) -> String {
    let mut out = String::new();

    for change in &plan.changes {
        if change.action == Action::NoChange && !verbose {
            continue;
        }
        let _ = writeln!(out, "{} {}", symbol(change.action), title(change));

        for field in &change.fields {
            if field.op == Op::Same && !verbose {
                continue;
            }
            let _ = writeln!(out, "  {}", field_line(field));
        }
    }

    let destructive: Vec<&String> = plan.changes.iter().flat_map(|c| &c.destructive).collect();
    if !destructive.is_empty() {
        out.push('\n');
        for message in destructive {
            let _ = writeln!(out, "Warning: {message}");
        }
    }

    if !plan.ignored.is_empty() {
        let _ = write!(
            out,
            "\nNote: {} undeclared resource(s) ({}) left unchanged; use --prune to delete them\n",
            plan.ignored.len(),
            plan.ignored.join(", ")
        );
    }

    let s = &plan.summary;
    if !out.is_empty() {
        out.push('\n');
    }
    if s.add + s.change + s.destroy == 0 {
        out.push_str("No changes.\n");
    } else {
        let _ = writeln!(
            out,
            "Plan: {} to add, {} to change, {} to destroy.",
            s.add, s.change, s.destroy
        );
    }
    out
}

fn field_line(field: &FieldChange) -> String {
    let path = &field.path;
    let value = |v: &Option<Json>| v.as_ref().map(|v| show(path, v)).unwrap_or_default();
    match field.op {
        Op::Same => format!("  {path}: {}", value(&field.after)),
        Op::Add => format!("+ {path}: {}", value(&field.after)),
        Op::Remove => format!("- {path}: {}", value(&field.before)),
        Op::Change => format!(
            "~ {path}: {} -> {}",
            value(&field.before),
            value(&field.after)
        ),
    }
}

/// Shows a value on one line. bypass actors and status checks are unpacked into a readable form.
fn show(path: &str, v: &Json) -> String {
    if path == "bypass_actors" {
        let mut s = v["type"].as_str().unwrap_or_default().to_string();
        if let Some(name) = v["name"].as_str() {
            s = format!("{s} {}", quote(name));
        }
        return format!("{s} ({})", v["mode"].as_str().unwrap_or_default());
    }
    if path.ends_with("required_status_checks.checks")
        && let Some(context) = v["context"].as_str()
    {
        return match v["integration"].as_str() {
            Some(app) => format!("{} ({app})", quote(context)),
            None => quote(context),
        };
    }
    v.to_string()
}

fn quote(s: &str) -> String {
    Json::String(s.to_string()).to_string()
}
