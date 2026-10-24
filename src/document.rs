//! Key paths of diagnostics (`ruleset."release/*".bypass_actors[1]`) resolved against the text of
//! the declaration file: where they are written, and suggested fixes applied to them.

use anyhow::{Context, Result, bail};
use serde_json::Value as Json;
use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::diagnostic::{Diagnostic, Edit, Location};

#[derive(Debug, PartialEq)]
enum Segment {
    Key(String),
    Index(usize),
}

/// Split a key path into keys and array indices. A quoted key is a JSON string, as `entry_path`
/// writes it.
fn segments(path: &str) -> Option<Vec<Segment>> {
    let mut out = Vec::new();
    let mut rest = path;
    loop {
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = closing_quote(quoted)?;
            out.push(Segment::Key(serde_json::from_str(&rest[..end + 2]).ok()?));
            rest = &quoted[end + 1..];
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            out.push(Segment::Key(rest[..end].to_string()));
            rest = &rest[end..];
        }
        while let Some(index) = rest.strip_prefix('[') {
            let (index, after) = index.split_once(']')?;
            out.push(Segment::Index(index.parse().ok()?));
            rest = after;
        }
        match rest.strip_prefix('.') {
            Some(after) => rest = after,
            None if rest.is_empty() => return Some(out),
            None => return None,
        }
    }
}

/// Byte offset of the quote closing a JSON string whose opening quote is already stripped.
fn closing_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(i),
            _ => escaped = false,
        }
    }
    None
}

/// A node of a document on the way down a key path.
enum Node<'a> {
    Table(&'a dyn TableLike),
    Array(&'a toml_edit::Array),
    Tables(&'a toml_edit::ArrayOfTables),
    Leaf,
}

impl<'a> Node<'a> {
    fn of_item(item: &'a Item) -> Self {
        match item {
            Item::Table(table) => Node::Table(table),
            Item::ArrayOfTables(tables) => Node::Tables(tables),
            Item::Value(value) => Node::of_value(value),
            Item::None => Node::Leaf,
        }
    }

    fn of_value(value: &'a Value) -> Self {
        match value {
            Value::InlineTable(table) => Node::Table(table),
            Value::Array(array) => Node::Array(array),
            _ => Node::Leaf,
        }
    }
}

/// Byte range where `path` is written: its key, or the element of an array. A path that is not
/// written (an unwritten setting) resolves to the closest table above it that is.
fn span(doc: &toml_edit::Document<&str>, path: &str) -> Option<std::ops::Range<usize>> {
    resolve(doc, path).0
}

/// [`span`], with whether it is the span of `path` itself rather than of a table above it.
fn resolve(doc: &toml_edit::Document<&str>, path: &str) -> (Option<std::ops::Range<usize>>, bool) {
    let Some(segments) = segments(path) else {
        return (None, false);
    };
    let mut node = Node::Table(doc.as_table());
    let mut found = None;
    for segment in segments {
        let step = match (&node, &segment) {
            (Node::Table(table), Segment::Key(key)) => table
                .get_key_value(key)
                .map(|(key, item)| (key.span().or_else(|| item.span()), Node::of_item(item))),
            (Node::Array(array), Segment::Index(i)) => array
                .get(*i)
                .map(|value| (value.span(), Node::of_value(value))),
            (Node::Tables(tables), Segment::Index(i)) => tables
                .get(*i)
                .map(|table| (table.span(), Node::Table(table))),
            _ => None,
        };
        let Some((span, next)) = step else {
            return (found, false);
        };
        found = span.or(found);
        node = next;
    }
    (found, true)
}

/// Line and column (both 1-based, the column counted in characters) of byte `offset` in `text`.
pub fn location(text: &str, offset: usize) -> Location {
    let before = &text[..offset.min(text.len())];
    Location {
        line: before.matches('\n').count() + 1,
        column: before[before.rfind('\n').map_or(0, |i| i + 1)..]
            .chars()
            .count()
            + 1,
    }
}

/// Fill in where each diagnostic with a path but no location is written.
pub fn locate(diagnostics: &mut [Diagnostic], text: &str) {
    let Ok(doc) = toml_edit::Document::parse(text) else {
        return;
    };
    for d in diagnostics.iter_mut().filter(|d| d.location.is_none()) {
        if let Some(span) = d.path.as_deref().and_then(|path| span(&doc, path)) {
            d.location = Some(location(text, span.start));
        }
    }
}

/// `text` with `edit` applied, keeping its comments and formatting.
pub fn apply(text: &str, edit: &Edit) -> Result<String> {
    match edit {
        // Done on the text, since toml_edit keeps the formatting of a key only when it keeps the
        // key itself, and moves a key it inserts to the end of its table
        Edit::Rename { path, to } => {
            let doc = toml_edit::Document::parse(text)?;
            let key = match (segments(path).as_deref(), resolve(&doc, path)) {
                (Some([.., Segment::Key(_)]), (Some(span), true)) => span,
                _ => bail!("{path} is not a written key"),
            };
            let to = toml_edit::Key::new(to.as_str()).display_repr().into_owned();
            Ok(format!("{}{to}{}", &text[..key.start], &text[key.end..]))
        }
        Edit::Remove { path } => edit_parent(text, path, |parent, last| match (parent, last) {
            (Parent::Table(table), Segment::Key(key)) => table.remove(key).map(drop),
            (Parent::Array(array), Segment::Index(i)) if *i < array.len() => {
                array.remove(*i);
                Some(())
            }
            (Parent::Tables(tables), Segment::Index(i)) if *i < tables.len() => {
                tables.remove(*i);
                Some(())
            }
            _ => None,
        }),
        Edit::Set { path, value } => {
            let new = to_value(value).with_context(|| format!("cannot write {value} in TOML"))?;
            edit_parent(text, path, |parent, last| {
                let old = match (parent, last) {
                    (Parent::Table(table), Segment::Key(key)) => match table.get_mut(key) {
                        Some(item) => item.as_value_mut()?,
                        None => {
                            table.insert(key, Item::Value(new.clone()));
                            return Some(());
                        }
                    },
                    (Parent::Array(array), Segment::Index(i)) => array.get_mut(*i)?,
                    _ => return None,
                };
                let decor = old.decor().clone();
                *old = new.clone();
                *old.decor_mut() = decor;
                Some(())
            })
        }
    }
}

enum Parent<'a> {
    Table(&'a mut dyn TableLike),
    Array(&'a mut toml_edit::Array),
    Tables(&'a mut toml_edit::ArrayOfTables),
    Leaf,
}

impl<'a> Parent<'a> {
    fn of_item(item: &'a mut Item) -> Self {
        match item {
            Item::Table(table) => Parent::Table(table),
            Item::ArrayOfTables(tables) => Parent::Tables(tables),
            Item::Value(value) => Parent::of_value(value),
            Item::None => Parent::Leaf,
        }
    }

    fn of_value(value: &'a mut Value) -> Self {
        match value {
            Value::InlineTable(table) => Parent::Table(table),
            Value::Array(array) => Parent::Array(array),
            _ => Parent::Leaf,
        }
    }

    fn step(self, segment: &Segment) -> Option<Self> {
        match (self, segment) {
            (Parent::Table(table), Segment::Key(key)) => table.get_mut(key).map(Parent::of_item),
            (Parent::Array(array), Segment::Index(i)) => array.get_mut(*i).map(Parent::of_value),
            (Parent::Tables(tables), Segment::Index(i)) => {
                tables.get_mut(*i).map(|table| Parent::Table(table))
            }
            _ => None,
        }
    }
}

/// Apply `f` to the table or array holding the last segment of `path`, and render the document.
/// `f` returns `None` when the segment does not fit there.
fn edit_parent(
    text: &str,
    path: &str,
    f: impl FnOnce(Parent<'_>, &Segment) -> Option<()>,
) -> Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    let segments = segments(path).with_context(|| format!("{path} is not a key path"))?;
    let Some((last, init)) = segments.split_last() else {
        bail!("{path} is not a key path");
    };
    let mut parent = Parent::Table(doc.as_table_mut());
    for segment in init {
        parent = parent
            .step(segment)
            .with_context(|| format!("{path} is not written"))?;
    }
    f(parent, last).with_context(|| format!("{path} is not written"))?;
    Ok(doc.to_string())
}

fn to_value(json: &Json) -> Option<Value> {
    Some(match json {
        Json::Bool(b) => Value::from(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::from(i),
            None => Value::from(n.as_f64()?),
        },
        Json::String(s) => Value::from(s.as_str()),
        Json::Array(values) => Value::Array(values.iter().map(to_value).collect::<Option<_>>()?),
        Json::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in map {
                table.insert(key, to_value(value)?);
            }
            Value::InlineTable(table)
        }
        Json::Null => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(k: &str) -> Segment {
        Segment::Key(k.to_string())
    }

    #[test]
    fn splits_key_paths() {
        assert_eq!(
            segments("ruleset.\"release/*.x\".bypass_actors[1].name"),
            Some(vec![
                key("ruleset"),
                key("release/*.x"),
                key("bypass_actors"),
                Segment::Index(1),
                key("name"),
            ])
        );
        assert_eq!(
            segments("ruleset.\"a\\\"b\".rules"),
            Some(vec![key("ruleset"), key("a\"b"), key("rules")])
        );
        assert_eq!(segments("a..b"), None);
        assert_eq!(segments("a[x]"), None);
    }

    fn line_of(text: &str, path: &str) -> Option<usize> {
        let doc = toml_edit::Document::parse(text).unwrap();
        span(&doc, path).map(|span| location(text, span.start).line)
    }

    #[test]
    fn locates_keys_elements_and_the_closest_written_table() {
        let text = "[repository]\nhas_wiki = false\n\n[ruleset.main]\nbypass_actors = [\n  \
            { type = \"a\" },\n  { type = \"b\" },\n]\n\n[[ruleset.tags.bypass_actors]]\n\
            type = \"c\"\n";
        assert_eq!(line_of(text, "repository.has_wiki"), Some(2));
        assert_eq!(line_of(text, "repository.homepage"), Some(1));
        assert_eq!(line_of(text, "ruleset.main.bypass_actors[1]"), Some(7));
        assert_eq!(
            line_of(text, "ruleset.tags.bypass_actors[0].type"),
            Some(11)
        );
        assert_eq!(line_of(text, "actions"), None);
        assert_eq!(
            location("a\nあい = 1", "a\nあい".len()),
            Location { line: 2, column: 3 }
        );
    }

    #[test]
    fn removes_keeping_the_rest_of_the_file() {
        let text = "# keep\n[ruleset.main]\nbypass_actors = [\n  { type = \"a\" }, # first\n  \
            { type = \"b\" },\n]\n";
        let fixed = apply(
            text,
            &Edit::Remove {
                path: "ruleset.main.bypass_actors[0]".into(),
            },
        )
        .unwrap();
        assert!(fixed.starts_with("# keep\n[ruleset.main]\n"), "{fixed}");
        assert!(!fixed.contains("\"a\""), "{fixed}");
        assert!(fixed.contains("{ type = \"b\" }"), "{fixed}");

        let text = "[[ruleset.main.bypass_actors]]\ntype = \"a\"\n\n\
            [[ruleset.main.bypass_actors]]\ntype = \"b\"\n";
        let fixed = apply(
            text,
            &Edit::Remove {
                path: "ruleset.main.bypass_actors[1]".into(),
            },
        )
        .unwrap();
        assert_eq!(fixed, "[[ruleset.main.bypass_actors]]\ntype = \"a\"\n");

        let text = "[collaborator.octocat]\npermission = \"admin\" # why\n";
        let fixed = apply(
            text,
            &Edit::Set {
                path: "collaborator.octocat.permission".into(),
                value: json!("push"),
            },
        )
        .unwrap();
        assert_eq!(
            fixed,
            "[collaborator.octocat]\npermission = \"push\" # why\n"
        );
        assert!(
            apply(
                text,
                &Edit::Remove {
                    path: "collaborator.hubot".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn renames_in_place() {
        let text = "[ruleset.main.rules]\ndeletion = true # keep\nblock_force_pushes = true\n\n\
            [ruleset.main.rules.update]\nupdate_allows_fetch_and_merge = false\n";
        let rename = |path: &str, to: &str| Edit::Rename {
            path: path.into(),
            to: to.into(),
        };
        let fixed = apply(
            text,
            &rename("ruleset.main.rules.deletion", "restrict_deletions"),
        )
        .unwrap();
        assert!(
            fixed.starts_with("[ruleset.main.rules]\nrestrict_deletions = true # keep\nblock"),
            "{fixed}"
        );
        let fixed = apply(
            &fixed,
            &rename("ruleset.main.rules.update", "restrict_updates"),
        )
        .unwrap();
        assert!(
            fixed.contains("[ruleset.main.rules.restrict_updates]\n"),
            "{fixed}"
        );
        assert!(apply(text, &rename("ruleset.main.rules.missing", "x")).is_err());
    }
}
