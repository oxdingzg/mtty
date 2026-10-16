//! Reading the parts of server answers mtty uses: diagnostics, hover text,
//! completion items and definition locations.

use std::path::PathBuf;

use serde_json::Value;

use crate::position::{uri_to_path, LspRange};

/// 1 error, 2 warning, 3 information, 4 hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: LspRange,
    pub severity: u8,
    pub message: String,
    pub source: Option<String>,
}

/// `textDocument/publishDiagnostics`: the file and its diagnostics.
pub fn diagnostics(params: &Value) -> Option<(PathBuf, Vec<Diagnostic>)> {
    diagnostics_using(params, uri_to_path)
}

pub fn diagnostics_using(
    params: &Value,
    decode: impl Fn(&str) -> Option<PathBuf>,
) -> Option<(PathBuf, Vec<Diagnostic>)> {
    let path = decode(params.get("uri")?.as_str()?)?;
    let list = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .filter_map(|d| {
            Some(Diagnostic {
                range: LspRange::from_json(d.get("range")?)?,
                severity: d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) as u8,
                message: d.get("message")?.as_str()?.to_string(),
                source: d.get("source").and_then(|s| s.as_str()).map(str::to_string),
            })
        })
        .collect();
    Some((path, list))
}

/// A hover answer as Markdown, or `None` when there is nothing to show.
pub fn hover_text(result: &Value) -> Option<String> {
    fn part(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => {
                let value = o.get("value")?.as_str()?;
                match o.get("language").and_then(|l| l.as_str()) {
                    Some(lang) => Some(format!("```{lang}\n{value}\n```")),
                    None if o.get("kind").and_then(|k| k.as_str()) == Some("plaintext") => {
                        Some(format!("```text\n{value}\n```"))
                    }
                    None => Some(value.to_string()),
                }
            }
            _ => None,
        }
    }
    let contents = result.get("contents")?;
    let text = match contents {
        Value::Array(parts) => parts
            .iter()
            .filter_map(part)
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => part(other)?,
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Where a definition is: file and range, for each answer (a Location, a
/// list of them, or LocationLinks).
pub fn locations(result: &Value) -> Vec<(PathBuf, LspRange)> {
    locations_using(result, uri_to_path)
}

pub fn locations_using(
    result: &Value,
    decode: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<(PathBuf, LspRange)> {
    let one = |v: &Value| -> Option<(PathBuf, LspRange)> {
        if let Some(uri) = v.get("targetUri") {
            let range = v
                .get("targetSelectionRange")
                .or_else(|| v.get("targetRange"))?;
            return Some((decode(uri.as_str()?)?, LspRange::from_json(range)?));
        }
        Some((
            decode(v.get("uri")?.as_str()?)?,
            LspRange::from_json(v.get("range")?)?,
        ))
    };
    match result {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Null => Vec::new(),
        other => one(other).into_iter().collect(),
    }
}

/// A completion offered at the caret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    /// The protocol's item kind (3 function, 6 variable, 7 class, 14 keyword…).
    pub kind: u8,
    pub detail: Option<String>,
    /// Text matched against what was typed.
    pub filter: String,
    pub sort: String,
    /// What accepting writes (a snippet's placeholders already resolved).
    pub insert: String,
    /// Where the caret goes in `insert` (chars), when not at its end.
    pub cursor: Option<usize>,
    /// The range the text replaces, when the server gave one.
    pub range: Option<LspRange>,
    /// Edits elsewhere in the file (an import), applied too.
    pub additional: Vec<(LspRange, String)>,
}

/// The items of a completion answer, and whether the list is incomplete
/// (asked again as typing goes on).
pub fn completion_items(result: &Value) -> (Vec<CompletionItem>, bool) {
    let (items, incomplete) = match result {
        Value::Array(a) => (a.as_slice(), false),
        Value::Object(o) => (
            o.get("items")
                .and_then(|i| i.as_array())
                .map_or(&[][..], |a| a.as_slice()),
            o.get("isIncomplete")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        ),
        _ => (&[][..], false),
    };
    let items = items.iter().filter_map(completion_item).collect();
    (items, incomplete)
}

fn completion_item(v: &Value) -> Option<CompletionItem> {
    let label = v.get("label")?.as_str()?.to_string();
    let snippet = v.get("insertTextFormat").and_then(|f| f.as_u64()) == Some(2);
    let edit = v.get("textEdit");
    let (raw, range) = match edit {
        Some(e) => (
            e.get("newText")?.as_str()?.to_string(),
            // An InsertReplaceEdit: insert, as typing goes on.
            LspRange::from_json(e.get("range").or_else(|| e.get("insert"))?),
        ),
        None => (
            v.get("insertText")
                .and_then(|t| t.as_str())
                .unwrap_or(&label)
                .to_string(),
            None,
        ),
    };
    let (insert, cursor) = if snippet {
        snippet_to_text(&raw)
    } else {
        (raw, None)
    };
    let additional = v
        .get("additionalTextEdits")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some((
                        LspRange::from_json(e.get("range")?)?,
                        e.get("newText")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(CompletionItem {
        filter: v
            .get("filterText")
            .and_then(|t| t.as_str())
            .unwrap_or(&label)
            .to_string(),
        sort: v
            .get("sortText")
            .and_then(|t| t.as_str())
            .unwrap_or(&label)
            .to_string(),
        kind: v.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8,
        detail: v.get("detail").and_then(|d| d.as_str()).map(str::to_string),
        label,
        insert,
        cursor,
        range,
        additional,
    })
}

/// A snippet as plain text: placeholders keep their default text, choices
/// their first option, variables their default (or nothing). The caret
/// goes to the first tabstop (`$1`), else `$0`, else the end.
pub fn snippet_to_text(snippet: &str) -> (String, Option<usize>) {
    let chars: Vec<char> = snippet.chars().collect();
    let mut out = String::new();
    let mut stops: Vec<(u32, usize)> = Vec::new();
    parse_snippet(&chars, &mut 0, &mut out, &mut stops, false);
    let first = stops
        .iter()
        .filter(|(n, _)| *n > 0)
        .min_by_key(|(n, _)| *n)
        .or_else(|| stops.iter().find(|(n, _)| *n == 0))
        .map(|(_, at)| *at);
    let len = out.chars().count();
    (out, first.filter(|at| *at < len))
}

/// Parse until the end, or a `}` that closes a placeholder when `nested`.
fn parse_snippet(
    c: &[char],
    i: &mut usize,
    out: &mut String,
    stops: &mut Vec<(u32, usize)>,
    nested: bool,
) {
    while *i < c.len() {
        let ch = c[*i];
        match ch {
            '\\' if *i + 1 < c.len() && matches!(c[*i + 1], '$' | '}' | '\\' | ',' | '|') => {
                out.push(c[*i + 1]);
                *i += 2;
            }
            '}' if nested => return,
            '$' if *i + 1 < c.len() && c[*i + 1].is_ascii_digit() => {
                *i += 1;
                let n = read_number(c, i);
                stops.push((n, out.chars().count()));
            }
            '$' if *i + 1 < c.len() && c[*i + 1] == '{' => {
                *i += 2;
                if *i < c.len() && c[*i].is_ascii_digit() {
                    let n = read_number(c, i);
                    stops.push((n, out.chars().count()));
                    match c.get(*i) {
                        Some(':') => {
                            *i += 1;
                            parse_snippet(c, i, out, stops, true);
                        }
                        Some('|') => {
                            *i += 1;
                            // The first choice, up to `,` or `|`.
                            while *i < c.len() && c[*i] != ',' && c[*i] != '|' {
                                if c[*i] == '\\' && *i + 1 < c.len() {
                                    *i += 1;
                                }
                                out.push(c[*i]);
                                *i += 1;
                            }
                            while *i < c.len() && c[*i] != '}' {
                                *i += 1;
                            }
                        }
                        _ => {}
                    }
                } else {
                    // A variable: its default, if any.
                    while *i < c.len() && (c[*i].is_ascii_alphanumeric() || c[*i] == '_') {
                        *i += 1;
                    }
                    if c.get(*i) == Some(&':') {
                        *i += 1;
                        parse_snippet(c, i, out, stops, true);
                    } else {
                        while *i < c.len() && c[*i] != '}' {
                            *i += 1;
                        }
                    }
                }
                // The closing brace.
                if *i < c.len() && c[*i] == '}' {
                    *i += 1;
                }
            }
            '$' if *i + 1 < c.len() && (c[*i + 1].is_ascii_alphabetic() || c[*i + 1] == '_') => {
                // `$VAR`: nothing.
                *i += 1;
                while *i < c.len() && (c[*i].is_ascii_alphanumeric() || c[*i] == '_') {
                    *i += 1;
                }
            }
            _ => {
                out.push(ch);
                *i += 1;
            }
        }
    }
}

fn read_number(c: &[char], i: &mut usize) -> u32 {
    let mut n = 0u32;
    while *i < c.len() && c[*i].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add(c[*i] as u32 - '0' as u32);
        *i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snippets_become_text_with_the_caret_at_the_first_stop() {
        assert_eq!(snippet_to_text("foo($1)$0"), ("foo()".into(), Some(4)));
        assert_eq!(
            snippet_to_text("println!(\"${1:msg}\", ${2:x})"),
            ("println!(\"msg\", x)".into(), Some(10))
        );
        assert_eq!(
            snippet_to_text("a${1|one,two|}b"),
            ("aoneb".into(), Some(1))
        );
        assert_eq!(
            snippet_to_text("${1:outer ${2:inner}}"),
            ("outer inner".into(), Some(0))
        );
        assert_eq!(snippet_to_text("cost \\$5$0"), ("cost $5".into(), None));
        assert_eq!(
            snippet_to_text("${TM_FILENAME:file}.rs"),
            ("file.rs".into(), None)
        );
        assert_eq!(snippet_to_text("plain"), ("plain".into(), None));
    }

    #[test]
    fn hover_contents_in_every_shape() {
        let md = json!({"contents": {"kind": "markdown", "value": "**x**"}});
        assert_eq!(hover_text(&md).unwrap(), "**x**");
        let marked = json!({"contents": [{"language": "rust", "value": "fn f()"}, "docs"]});
        assert_eq!(hover_text(&marked).unwrap(), "```rust\nfn f()\n```\n\ndocs");
        assert_eq!(hover_text(&json!({"contents": ""})), None);
        assert_eq!(hover_text(&json!(null)), None);
    }

    #[test]
    fn locations_and_links() {
        let loc = json!({"uri": "file:///a.rs", "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}}});
        let found = locations(&loc);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, PathBuf::from("/a.rs"));
        assert_eq!(found[0].1.start.line, 3);
        let link = json!([{"targetUri": "file:///b.ts", "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}}, "targetSelectionRange": {"start": {"line": 2, "character": 6}, "end": {"line": 2, "character": 9}}}]);
        assert_eq!(
            locations(&link)[0].1.start.character,
            6,
            "the selection range"
        );
        assert!(locations(&json!(null)).is_empty());
    }

    #[test]
    fn completion_items_with_edits_and_snippets() {
        let list = json!({"isIncomplete": true, "items": [
            {"label": "push", "kind": 2, "detail": "fn(&mut self, T)",
             "insertTextFormat": 2,
             "textEdit": {"range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 4}}, "newText": "push(${1:value})$0"},
             "additionalTextEdits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "use x;\n"}]},
            {"label": "len", "sortText": "0001"},
        ]});
        let (items, incomplete) = completion_items(&list);
        assert!(incomplete);
        assert_eq!(items[0].insert, "push(value)");
        assert_eq!(items[0].cursor, Some(5));
        assert_eq!(items[0].range.unwrap().start.character, 2);
        assert_eq!(items[0].additional[0].1, "use x;\n");
        assert_eq!(
            (items[1].insert.as_str(), items[1].sort.as_str()),
            ("len", "0001")
        );
        let (plain, incomplete) = completion_items(&json!([{"label": "a"}]));
        assert!(!incomplete && plain.len() == 1);
    }

    #[test]
    fn published_diagnostics_default_to_errors() {
        let (path, list) = diagnostics(&json!({"uri": "file:///x.ts", "diagnostics": [
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "message": "bad", "source": "ts"},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}}, "message": "meh", "severity": 2},
        ]}))
        .unwrap();
        assert_eq!(path, PathBuf::from("/x.ts"));
        assert_eq!((list[0].severity, list[1].severity), (1, 2));
        assert_eq!(list[0].source.as_deref(), Some("ts"));
    }
}
