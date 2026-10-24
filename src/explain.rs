//! `ghrs explain`: the meaning of each key of the declaration file, read from the JSON Schema so
//! that it never disagrees with `ghrs schema`.

use anyhow::{Context, Result, bail};
use serde_json::Value as Json;

use crate::model;

/// Stands for a segment the user names, such as a ruleset name or a login.
const NAME: &str = "<name>";

/// How to go further from [`index`]. Written to stderr, so the list stays one key per line.
pub const HINT: &str = concat!(
    "\nExamples:\n",
    "  ghrs explain repository.visibility              # one key in detail\n",
    "  ghrs explain ruleset.main.rules.pull_request    # a rule and its parameters\n",
    "  ghrs explain --config .ghrs.toml                # each key of a file\n",
);

/// Every key, one per line with the first sentence of its description.
pub fn index() -> String {
    let root = model::json_schema();
    let mut out = String::new();
    list(&root, &root, "", &mut out);
    out
}

fn list(root: &Json, schema: &Json, path: &str, out: &mut String) {
    for (key, child) in children(root, schema) {
        let path = join(path, key);
        out.push_str(&format!(
            "{path}  {}\n",
            summary(&descriptions(root, child))
        ));
        list(root, child, &path, out);
    }
}

/// Everything the schema says about one key. A segment that names something (`ruleset.main`)
/// matches by any name, and so do `<name>` and `*`. Array indexes (`bypass_actors[0]`) are
/// ignored, since every item has the same keys.
pub fn key(path: &str) -> Result<String> {
    let root = model::json_schema();
    let (at, schema) = find(&root, path)?;

    let mut out = format!("{at}\n");
    out.push_str(&format!("  type: {}\n", type_name(&root, schema)));
    let resolved = resolve(&root, schema);
    let allowed = [schema, resolved, &resolved["items"]]
        .into_iter()
        .find_map(|s| s["enum"].as_array());
    if let Some(allowed) = allowed {
        let allowed: Vec<String> = allowed.iter().map(Json::to_string).collect();
        out.push_str(&format!("  allowed: {}\n", allowed.join(", ")));
    }
    if let Some(default) = schema.get("default") {
        out.push_str(&format!("  default: {default}\n"));
    }
    if let Some(name) = extension(schema, resolved, "x-github-api-name").and_then(Json::as_str) {
        out.push_str(&format!("  GitHub API name: {name}\n"));
    }
    if let Some(source) = extension(schema, resolved, "x-github-source").and_then(Json::as_str) {
        out.push_str(&format!("  GitHub API source: {source}\n"));
    }
    if extension(schema, resolved, "x-github-observed") == Some(&Json::Bool(true)) {
        out.push_str("  observed: returned by the API but not in its REST API description\n");
    }
    let keys: Vec<&str> = children(&root, schema)
        .iter()
        .map(|(key, _)| *key)
        .collect();
    if !keys.is_empty() {
        out.push_str(&format!("  keys: {}\n", keys.join(", ")));
    }
    for paragraph in descriptions(&root, schema) {
        out.push_str(&format!("\n  {paragraph}\n"));
    }
    Ok(out)
}

/// The key `path` names, as the schema writes it, and its schema.
fn find<'a>(root: &'a Json, path: &str) -> Result<(String, &'a Json)> {
    let mut schema = root;
    let mut at = String::new();
    for segment in path.split('.') {
        let segment = segment.split('[').next().unwrap_or_default();
        let children = children(root, schema);
        let found = children
            .iter()
            .find(|(key, _)| *key == segment)
            .or_else(|| children.iter().find(|(key, _)| *key == NAME));
        let Some((key, child)) = found else {
            let keys: Vec<&str> = children.iter().map(|(key, _)| *key).collect();
            bail!(
                "unknown key {segment:?} under {:?}; the keys there are: {}",
                if at.is_empty() { "the top level" } else { &at },
                keys.join(", ")
            );
        };
        at = join(&at, key);
        schema = child;
    }
    Ok((at, schema))
}

/// Each key written in a declaration file, with its value and description. Keys the schema does
/// not know are marked, so the output also shows what plan would reject.
pub fn config(text: &str) -> Result<String> {
    let value: toml::Value = toml::from_str(text).context("not a valid TOML document")?;
    let root = model::json_schema();
    let mut out = String::new();
    annotate(&root, Some(&root), &value, "", &mut out);
    Ok(out)
}

fn annotate(root: &Json, schema: Option<&Json>, value: &toml::Value, path: &str, out: &mut String) {
    match value {
        toml::Value::Table(table) => {
            let children = schema.map(|s| children(root, s)).unwrap_or_default();
            for (key, item) in table {
                let child = children
                    .iter()
                    .find(|(k, _)| k == key)
                    .or_else(|| children.iter().find(|(k, _)| *k == NAME))
                    .map(|(_, child)| *child);
                let at = join(path, key);
                if !item.is_table() && !is_table_array(item) {
                    out.push_str(&format!("{at} = {item}\n"));
                } else {
                    out.push_str(&format!("[{at}]\n"));
                }
                match child {
                    Some(child) => {
                        for paragraph in descriptions(root, child) {
                            out.push_str(&format!("  {paragraph}\n"));
                        }
                    }
                    None => out.push_str("  unknown key\n"),
                }
                annotate(root, child, item, &at, out);
            }
        }
        toml::Value::Array(items) if is_table_array(value) => {
            for (i, item) in items.iter().enumerate() {
                annotate(root, schema, item, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

fn is_table_array(value: &toml::Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
}

/// The keys under `schema`. A table keyed by name has the single key [`NAME`], and an array has
/// the keys of its items.
fn children<'a>(root: &'a Json, schema: &'a Json) -> Vec<(&'a str, &'a Json)> {
    let schema = resolve(root, schema);
    if let Some(any_of) = schema["anyOf"].as_array() {
        return any_of
            .iter()
            .flat_map(|branch| children(root, branch))
            .collect();
    }
    if let Some(items) = schema.get("items") {
        return children(root, items);
    }
    let mut out: Vec<(&str, &Json)> = schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, child)| (key.as_str(), child))
        .collect();
    if schema["additionalProperties"].is_object() {
        out.push((NAME, &schema["additionalProperties"]));
    }
    out
}

fn resolve<'a>(root: &'a Json, schema: &'a Json) -> &'a Json {
    match schema["$ref"].as_str() {
        Some(r) => &root["$defs"][r.trim_start_matches("#/$defs/")],
        None => schema,
    }
}

/// A keyword set on the key itself, or else on the type it refers to.
fn extension<'a>(schema: &'a Json, resolved: &'a Json, keyword: &str) -> Option<&'a Json> {
    schema.get(keyword).or_else(|| resolved.get(keyword))
}

fn type_name(root: &Json, schema: &Json) -> String {
    let schema = resolve(root, schema);
    if let Some(any_of) = schema["anyOf"].as_array() {
        let names: Vec<String> = any_of.iter().map(|b| type_name(root, b)).collect();
        return names.join(" or ");
    }
    match schema["type"].as_str() {
        Some("array") => format!("array of {}", type_name(root, &schema["items"])),
        Some("object") if schema["additionalProperties"].is_object() => {
            format!(
                "{} keyed by name",
                type_name(root, &schema["additionalProperties"])
            )
        }
        Some("object") => "table".to_string(),
        Some(name) => name.to_string(),
        None if schema.get("properties").is_some() => "table".to_string(),
        None => "any".to_string(),
    }
}

/// Paragraphs describing the key, then the types it refers to. Doc comments are wrapped to the
/// line length, so the lines of a paragraph are joined back.
fn descriptions(root: &Json, schema: &Json) -> Vec<String> {
    let resolved = resolve(root, schema);
    let mut texts = vec![schema["description"].as_str()];
    if !std::ptr::eq(resolved, schema) {
        texts.push(resolved["description"].as_str());
    }
    for branch in resolved["anyOf"].as_array().into_iter().flatten() {
        texts.push(resolve(root, branch)["description"].as_str());
    }
    texts
        .into_iter()
        .flatten()
        .flat_map(|text| text.split("\n\n"))
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

fn summary(descriptions: &[String]) -> &str {
    let first = descriptions.first().map_or("", String::as_str);
    first.find(". ").map_or(first, |end| &first[..=end])
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_lists_every_key_with_a_summary() {
        let index = index();
        for line in index.lines() {
            let (path, summary) = line.split_once("  ").unwrap();
            assert!(!summary.is_empty(), "{path}");
        }
        for rule in model::Rules::KEYS {
            let rule = crate::schema::rule_to_toml(rule);
            assert!(
                index.contains(&format!("\nruleset.<name>.rules.{rule}  ")),
                "{rule}"
            );
        }
    }

    #[test]
    fn hint_examples_are_keys() {
        let examples: Vec<&str> = HINT
            .lines()
            .filter_map(|line| line.strip_prefix("  ghrs explain "))
            .filter_map(|line| line.split(" #").next())
            .map(str::trim)
            .filter(|example| !example.starts_with("--"))
            .collect();
        assert_eq!(examples.len(), 2);
        for example in examples {
            assert!(key(example).is_ok(), "{example}");
        }
    }

    #[test]
    fn key_follows_names_rules_and_arrays() {
        let text = key("ruleset.main.rules.pull_request.required_approving_review_count").unwrap();
        assert!(text.starts_with(
            "ruleset.<name>.rules.pull_request.required_approving_review_count\n  type: integer\n"
        ));
        assert!(text.contains("approving reviews"));

        let text = key("ruleset.*.bypass_actors[0].type").unwrap();
        assert!(text.contains("  GitHub API name: actor_type\n"));
        assert!(text.contains("  allowed: \"integration\""));

        let text = key("ruleset.main.rules.required_status_checks").unwrap();
        assert!(text.contains("  type: boolean or table\n"));
        assert!(text.contains("  keys: checks, do_not_enforce_on_create, require_up_to_date\n"));

        let text = key("ruleset.main.rules.pull_request.allowed_merge_methods").unwrap();
        assert!(text.contains("  allowed: \"merge\", \"squash\", \"rebase\"\n"));
    }

    #[test]
    fn key_names_the_keys_where_it_stops() {
        let err = key("ruleset.main.rules.block_force_push")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with(
            "unknown key \"block_force_push\" under \"ruleset.<name>.rules\"; the keys there are: "
        ));
        assert!(err.contains("block_force_pushes"));
    }

    #[test]
    fn config_describes_each_written_key() {
        let text = "[repository]\nhas_wiki = false\nhas_wikis = true\n\
            [ruleset.main]\ntarget = \"branch\"\nenforcement = \"active\"\n\
            bypass_actors = [{ type = \"repository_role\", name = \"admin\" }]\n";
        let out = config(text).unwrap();
        assert!(out.contains("[repository]\n  Repository settings."));
        assert!(out.contains("repository.has_wiki = false\n  Enables the wiki\n"));
        assert!(out.contains("repository.has_wikis = true\n  unknown key\n"));
        assert!(out.contains("ruleset.main.target = \"branch\"\n"));
        assert!(out.contains("[ruleset.main.bypass_actors]\n  Actors that may bypass"));
        assert!(out.contains("ruleset.main.bypass_actors[0].type = \"repository_role\"\n"));
    }

    #[test]
    fn config_knows_every_key_of_the_readme_sample() {
        let readme = include_str!("../README.md");
        let sample = readme
            .split("## Declaration file")
            .nth(1)
            .and_then(|s| s.split("```toml\n").nth(1))
            .and_then(|s| s.split("```").next())
            .unwrap();
        assert!(!config(sample).unwrap().contains("unknown key"));
    }
}
