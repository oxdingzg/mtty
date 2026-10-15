//! File/line references from compiler output, independent of local existence.
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLink {
    pub path: String,
    pub line: usize,
    /// Compiler columns are one-based; callers convert to editor coordinates.
    pub column: Option<usize>,
    pub start: u16,
    pub end: u16,
}

fn position(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

pub fn parse(text: &str) -> Option<FileLink> {
    let text = text.trim_end_matches(['.', ',', ';', ':', ']', '}']);
    let (path, line, column) = if let Some((path, coordinates)) = text
        .strip_suffix(')')
        .and_then(|text| text.rsplit_once('('))
    {
        let (line, column) = coordinates
            .split_once(',')
            .map_or((coordinates, None), |(line, column)| (line, Some(column)));
        let column = match column {
            Some(column) => Some(position(column)?),
            None => None,
        };
        (path, position(line)?, column)
    } else {
        let text = text.trim_end_matches(')');
        let (prefix, last) = text.rsplit_once(':')?;
        let last = position(last)?;
        if let Some((path, line)) = prefix
            .rsplit_once(':')
            .filter(|(_, line)| position(line).is_some())
        {
            (path, position(line)?, Some(last))
        } else {
            (prefix, last, None)
        }
    };
    let path = path.trim_matches(['\'', '"', '`']);
    if line == 0 || path.is_empty() || path.contains("://") {
        return None;
    }
    let plausible = path.contains(['/', '\\'])
        || path
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext.starts_with(char::is_alphabetic))
        || matches!(path, "Makefile" | "Dockerfile" | "CMakeLists.txt");
    plausible.then(|| FileLink {
        path: path.to_owned(),
        line,
        column,
        start: 0,
        end: 0,
    })
}

fn width(text: &str) -> u16 {
    text.width().min(u16::MAX as usize) as u16
}

pub fn at(text: &str, column: u16) -> Option<FileLink> {
    let mut offset = 0;
    while offset < text.len() {
        let ch = text[offset..].chars().next()?;
        if ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | '<') {
            offset += ch.len_utf8();
            continue;
        }
        let start = offset;
        if matches!(ch, '\'' | '"' | '`') {
            offset += ch.len_utf8();
            let close = text[offset..].find(ch)?;
            offset += close + ch.len_utf8();
        }
        while offset < text.len() {
            let ch = text[offset..].chars().next()?;
            if ch.is_whitespace() {
                break;
            }
            offset += ch.len_utf8();
        }
        let raw = &text[start..offset];
        if let Some(mut link) = parse(raw) {
            link.start = width(&text[..start]);
            link.end = link.start.saturating_add(width(raw)).saturating_sub(1);
            if column >= link.start && column <= link.end {
                return Some(link);
            }
        }
    }
    None
}

/// A POSIX remote path stays remote, including '~/' (expanded by the SSH
/// transfer helpers). Missing remote cwd never falls back to a local cwd.
pub fn remote_path(path: &str, cwd: Option<&str>) -> Result<String, String> {
    if path.starts_with('/') || path.starts_with("~/") || path == "~" {
        return Ok(path.to_owned());
    }
    let base = cwd
        .filter(|cwd| cwd.starts_with('/'))
        .ok_or("remote working directory unavailable; use an absolute path")?;
    let joined = format!("{}/{}", base.trim_end_matches('/'), path);
    let mut components = Vec::new();
    for component in joined.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }
    Ok(format!("/{}", components.join("/")))
}

pub fn local_path(path: &str, cwd: Option<&str>) -> PathBuf {
    let base = cwd.unwrap_or(".");
    let base = if cfg!(windows) && base.starts_with('/') && base.as_bytes().get(2) == Some(&b':') {
        &base[1..]
    } else {
        base
    };
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(base).join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_colon_msvc_quoted_and_unicode_references() {
        let link = parse("src/main.rs:12:3:").unwrap();
        assert_eq!(
            (&*link.path, link.line, link.column),
            ("src/main.rs", 12, Some(3))
        );
        let link = parse(r"C:\project\a.rs(24,5):").unwrap();
        assert_eq!(
            (&*link.path, link.line, link.column),
            (r"C:\project\a.rs", 24, Some(5))
        );
        let line = "错误: \"src/my file.rs\":9:2 message";
        let link = at(line, 12).unwrap();
        assert_eq!(
            (&*link.path, link.line, link.column),
            ("src/my file.rs", 9, Some(2))
        );
        assert_eq!(link.start, 6);
        for token in ["12:34", "https://host/a.rs:12", "file.rs:0", "file.rs:no"] {
            assert!(parse(token).is_none(), "{token}");
        }
    }
    #[test]
    fn relative_remote_paths_require_a_remote_directory() {
        assert_eq!(
            remote_path("../src/a.rs", Some("/project/build")).unwrap(),
            "/project/src/a.rs"
        );
        assert!(remote_path("src/a.rs", None).is_err());
        assert_eq!(remote_path("/src/a.rs", None).unwrap(), "/src/a.rs");
        assert_eq!(remote_path("~/src/a.rs", None).unwrap(), "~/src/a.rs");
    }
}
