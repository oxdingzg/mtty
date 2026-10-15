//! LSP positions and URIs. A position is a line and a column counted in
//! the encoding the server chose: UTF-16 code units (the protocol's
//! default) or UTF-8 bytes. The editor counts chars.

use std::path::{Path, PathBuf};

use mtty_editor::{layout, Rope};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    #[default]
    Utf16,
}

impl Encoding {
    /// The encoding a server announced (`positionEncoding`).
    pub fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("utf-8") => Encoding::Utf8,
            _ => Encoding::Utf16,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LspRange {
    pub start: Pos,
    pub end: Pos,
}

impl Pos {
    pub fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character })
    }

    pub fn from_json(v: &Value) -> Option<Pos> {
        Some(Pos {
            line: u32::try_from(v.get("line")?.as_u64()?).ok()?,
            character: u32::try_from(v.get("character")?.as_u64()?).ok()?,
        })
    }
}

impl LspRange {
    pub fn from_json(v: &Value) -> Option<LspRange> {
        Some(LspRange {
            start: Pos::from_json(v.get("start")?)?,
            end: Pos::from_json(v.get("end")?)?,
        })
    }
}

/// The position of char `at` in `rope`.
pub fn char_to_pos(rope: &Rope, at: usize, enc: Encoding) -> Pos {
    let at = at.min(rope.len_chars());
    let line = rope.char_to_line(at);
    let start = rope.line_to_char(line);
    let character = match enc {
        Encoding::Utf16 => rope.char_to_utf16_cu(at) - rope.char_to_utf16_cu(start),
        Encoding::Utf8 => rope.char_to_byte(at) - rope.char_to_byte(start),
    };
    Pos {
        line: line as u32,
        character: character as u32,
    }
}

/// The char a position names, clamped to the document and to its line's
/// end (before the line break), as the protocol asks.
pub fn pos_to_char(rope: &Rope, pos: Pos, enc: Encoding) -> usize {
    let lines = rope.len_lines();
    let line = pos.line as usize;
    if line >= lines {
        return rope.len_chars();
    }
    let start = rope.line_to_char(line);
    let end = start + layout::content_len(rope.line(line));
    let at = match enc {
        Encoding::Utf16 => {
            let cu = rope.char_to_utf16_cu(start) + pos.character as usize;
            rope.utf16_cu_to_char(cu.min(rope.len_utf16_cu()))
        }
        Encoding::Utf8 => {
            let byte = rope.char_to_byte(start) + pos.character as usize;
            rope.byte_to_char(byte.min(rope.len_bytes()))
        }
    };
    at.clamp(start, end)
}

/// Bytes a URI path keeps as they are.
fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/')
}

/// `file://` URI of an absolute path (percent-encoded; on Windows the
/// drive becomes `/C:/…`).
pub fn path_to_uri(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    let mut out = String::from("file://");
    for (i, b) in s.bytes().enumerate() {
        // A Windows drive's colon stays readable, as editors write it.
        if unreserved(b) || (b == b':' && i == 2) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path of a `file://` URI; `None` for other schemes.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An authority (`file://host/…`) other than empty or localhost is not ours.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    // `/C:/x` on Windows.
    if cfg!(windows) && s.len() > 2 && s.as_bytes()[2] == b':' {
        return Some(PathBuf::from(&s[1..]));
    }
    Some(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_count_surrogate_pairs() {
        // "a😀b": the emoji is one char, two UTF-16 units, four bytes.
        let rope = Rope::from_str("x\na😀b\n");
        let b = 4; // chars: x \n a 😀 b
        assert_eq!(
            char_to_pos(&rope, b, Encoding::Utf16),
            Pos {
                line: 1,
                character: 3
            }
        );
        assert_eq!(
            char_to_pos(&rope, b, Encoding::Utf8),
            Pos {
                line: 1,
                character: 5
            }
        );
        for enc in [Encoding::Utf16, Encoding::Utf8] {
            assert_eq!(pos_to_char(&rope, char_to_pos(&rope, b, enc), enc), b);
        }
    }

    #[test]
    fn positions_past_a_line_or_the_document_clamp() {
        let rope = Rope::from_str("ab\r\ncd");
        let p = |line, character| Pos { line, character };
        assert_eq!(
            pos_to_char(&rope, p(0, 99), Encoding::Utf16),
            2,
            "before CRLF"
        );
        assert_eq!(pos_to_char(&rope, p(1, 1), Encoding::Utf16), 5);
        assert_eq!(pos_to_char(&rope, p(7, 0), Encoding::Utf16), 6);
    }

    #[test]
    fn uris_round_trip_with_spaces_and_unicode() {
        let path = Path::new("/srv/my dir/中文#1.rs");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///srv/my%20dir/%E4%B8%AD%E6%96%87%231.rs");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
        assert_eq!(uri_to_path("https://x/y"), None);
        assert_eq!(
            uri_to_path("file://localhost/tmp/a.ts").unwrap(),
            Path::new("/tmp/a.ts")
        );
    }
}
