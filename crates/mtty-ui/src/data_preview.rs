//! Bounded, read-only previews for JSON, CSV/TSV and line-oriented logs.
use serde_json::Value;
use std::collections::BTreeSet;

pub const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
const MAX_COLUMNS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Csv,
    Tsv,
    Lines,
}
impl Format {
    pub fn for_path(path: &std::path::Path) -> Self {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "json" => Self::Json,
            "csv" => Self::Csv,
            "tsv" => Self::Tsv,
            _ => Self::Lines,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Lines => "Lines",
        }
    }
}
#[derive(Clone, Debug)]
pub enum Preview {
    Json(Value),
    Table(Table),
}
#[derive(Clone, Debug)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
    pub line_starts: Vec<usize>,
    pub columns: usize,
    pub truncated: bool,
}

pub fn parse(text: &str, format: Format) -> Result<Preview, String> {
    if text.len() > MAX_BYTES {
        return Err("data preview input exceeds 2 MiB".into());
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match format {
        Format::Json => serde_json::from_str(text)
            .map(Preview::Json)
            .map_err(|error| error.to_string()),
        Format::Lines => Ok(Preview::Table(Table {
            rows: text
                .lines()
                .take(MAX_ROWS)
                .map(|line| vec![line.to_owned()])
                .collect(),
            line_starts: (1..=text.lines().count().min(MAX_ROWS)).collect(),
            columns: 1,
            truncated: text.lines().count() > MAX_ROWS,
        })),
        Format::Csv | Format::Tsv => {
            csv(text, if format == Format::Tsv { '\t' } else { ',' }).map(Preview::Table)
        }
    }
}

fn csv(text: &str, delimiter: char) -> Result<Table, String> {
    let mut result = Table {
        rows: Vec::new(),
        line_starts: Vec::new(),
        columns: 0,
        truncated: false,
    };
    let (mut row, mut field) = (Vec::new(), String::new());
    let (mut quoted, mut closed, mut pending) = (false, false, false);
    let (mut physical, mut start, mut column, mut previous_cr) = (1, 1, 0, false);
    let mut chars = text.chars().peekable();
    fn push_field(
        table: &mut Table,
        row: &mut Vec<String>,
        field: &mut String,
        column: &mut usize,
    ) {
        *column += 1;
        if row.len() < MAX_COLUMNS {
            row.push(std::mem::take(field));
        } else {
            field.clear();
            table.truncated = true;
        }
        table.columns = table.columns.max(*column).min(MAX_COLUMNS);
    }
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            physical += 1;
            previous_cr = true;
        } else if ch == '\n' {
            if !previous_cr {
                physical += 1;
            }
            previous_cr = false;
        } else {
            previous_cr = false;
        }
        pending = true;
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(ch);
            }
        } else if ch == delimiter {
            push_field(&mut result, &mut row, &mut field, &mut column);
            closed = false;
        } else if matches!(ch, '\r' | '\n') {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            push_field(&mut result, &mut row, &mut field, &mut column);
            if result.rows.len() == MAX_ROWS {
                result.truncated = true;
                break;
            }
            result.rows.push(std::mem::take(&mut row));
            result.line_starts.push(start);
            start = physical;
            column = 0;
            pending = false;
            closed = false;
        } else if ch == '"' && field.is_empty() && !closed {
            quoted = true;
        } else if ch == '"' || closed {
            return Err(format!(
                "unexpected text/quote in CSV record {}",
                result.rows.len() + 1
            ));
        } else {
            field.push(ch);
        }
    }
    if quoted {
        return Err(format!(
            "unterminated quote in CSV record {}",
            result.rows.len() + 1
        ));
    }
    if pending && result.rows.len() < MAX_ROWS {
        push_field(&mut result, &mut row, &mut field, &mut column);
        result.rows.push(row);
        result.line_starts.push(start);
    } else if pending {
        result.truncated = true;
    }
    Ok(result)
}

#[derive(Clone, Debug)]
pub struct JsonRow {
    pub path: Vec<usize>,
    pub depth: usize,
    pub label: String,
    pub expandable: bool,
    pub expanded: bool,
}

pub fn json_rows(value: &Value, open: &BTreeSet<Vec<usize>>, query: &str) -> (Vec<JsonRow>, bool) {
    struct Walk<'a> {
        open: &'a BTreeSet<Vec<usize>>,
        query: String,
        rows: Vec<JsonRow>,
        visited: usize,
        truncated: bool,
    }
    impl Walk<'_> {
        fn visit(&mut self, value: &Value, name: &str, path: &mut Vec<usize>, force: bool) -> bool {
            if self.visited >= 20_000 || self.rows.len() >= MAX_ROWS {
                self.truncated = true;
                return false;
            }
            self.visited += 1;
            let own = force
                || self.query.is_empty()
                || name.to_lowercase().contains(&self.query)
                || match value {
                    Value::String(text) => text.to_lowercase().contains(&self.query),
                    Value::Number(number) => number.to_string().contains(&self.query),
                    Value::Bool(boolean) => boolean.to_string().contains(&self.query),
                    Value::Null => "null".contains(&self.query),
                    _ => false,
                };
            let expandable = matches!(value, Value::Array(values) if !values.is_empty())
                || matches!(value,Value::Object(values) if !values.is_empty());
            let expanded = expandable && (!self.query.is_empty() || self.open.contains(path));
            let short_name: String = name
                .chars()
                .take(160)
                .flat_map(char::escape_debug)
                .collect();
            let body = match value {
                Value::Object(values) => format!("{{ {} keys }}", values.len()),
                Value::Array(values) => format!("[ {} items ]", values.len()),
                Value::String(text) => {
                    let short: String = text.chars().take(512).collect();
                    format!(
                        "{}{}",
                        serde_json::to_string(&short).unwrap_or_default(),
                        if text.chars().nth(512).is_some() {
                            " …"
                        } else {
                            ""
                        }
                    )
                }
                _ => value.to_string().chars().take(512).collect(),
            };
            let at = self.rows.len();
            self.rows.push(JsonRow {
                path: path.clone(),
                depth: path.len(),
                label: format!("{short_name}: {body}"),
                expandable,
                expanded,
            });
            let mut child_match = false;
            if expanded {
                let include = force || own && !self.query.is_empty();
                match value {
                    Value::Array(values) => {
                        for (index, value) in values.iter().enumerate() {
                            path.push(index);
                            child_match |= self.visit(value, &index.to_string(), path, include);
                            path.pop();
                            if self.truncated {
                                break;
                            }
                        }
                    }
                    Value::Object(values) => {
                        for (index, (name, value)) in values.iter().enumerate() {
                            path.push(index);
                            child_match |= self.visit(value, name, path, include);
                            path.pop();
                            if self.truncated {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if own || child_match {
                true
            } else {
                self.rows.truncate(at);
                false
            }
        }
    }
    let mut walk = Walk {
        open,
        query: query.to_lowercase(),
        rows: Vec::new(),
        visited: 0,
        truncated: false,
    };
    walk.visit(value, "$", &mut Vec::new(), false);
    (walk.rows, walk.truncated)
}

pub fn json_value<'a>(value: &'a Value, path: &[usize]) -> Option<&'a Value> {
    let mut value = value;
    for index in path {
        value = match value {
            Value::Array(values) => values.get(*index)?,
            Value::Object(values) => values.values().nth(*index)?,
            _ => return None,
        };
    }
    Some(value)
}
pub fn json_pointer(value: &Value, path: &[usize]) -> Option<String> {
    let mut value = value;
    let mut pointer = String::new();
    for index in path {
        let (component, next) = match value {
            Value::Array(values) => (index.to_string(), values.get(*index)?),
            Value::Object(values) => {
                let (key, value) = values.iter().nth(*index)?;
                (key.replace('~', "~0").replace('/', "~1"), value)
            }
            _ => return None,
        };
        pointer.push('/');
        pointer.push_str(&component);
        value = next;
    }
    Some(pointer)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_handles_quoted_delimiters_unicode_newlines_and_source_line_mapping() {
        let data = csv(
            "name,note\r\n你好,\"a,b\r\nquoted \"\"text\"\"\"\r\nlast,\r\n",
            ',',
        )
        .unwrap();
        assert_eq!(data.rows[1], vec!["你好", "a,b\r\nquoted \"text\""]);
        assert_eq!(data.rows[2], vec!["last", ""]);
        assert_eq!(data.line_starts, vec![1, 2, 4]);
        assert!(csv("a,\"not closed", ',').is_err());
        assert!(csv("\"a\"bad,b", ',').is_err());
    }
    #[test]
    fn json_preserves_large_numbers_and_escapes_pointer_keys() {
        let Preview::Json(value) =
            parse("{\"a/b~c\":[123456789012345678901234567890]}", Format::Json).unwrap()
        else {
            panic!()
        };
        let pointer = json_pointer(&value, &[0, 0]).unwrap();
        assert_eq!(pointer, "/a~1b~0c/0");
        assert_eq!(
            value.pointer(&pointer).unwrap().to_string(),
            "123456789012345678901234567890"
        );
        let (rows, _) = json_rows(&value, &BTreeSet::new(), "123456");
        assert_eq!(rows.len(), 3);
    }
}
