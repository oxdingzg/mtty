//! Remote document identities and POSIX transport arguments.
use crate::ServerSettings;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    /// SSH executable/options/destination, without the remote command.
    pub prefix: Vec<String>,
    pub servers: HashMap<String, ServerSettings>,
}
fn hex(text: &str) -> String {
    text.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn unhex(text: &str) -> Option<String> {
    if text.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::new();
    for pair in text.as_bytes().chunks_exact(2) {
        bytes.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
    }
    String::from_utf8(bytes).ok()
}
pub fn document_key(target: &str, path: &str) -> PathBuf {
    PathBuf::from(format!("mtty-ssh://{}/{}", hex(target), hex(path)))
}
pub fn split_key(path: &Path) -> Option<(String, String)> {
    let (target, path) = path
        .to_str()?
        .strip_prefix("mtty-ssh://")?
        .split_once('/')?;
    Some((unhex(target)?, unhex(path)?))
}

pub fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
pub fn uri_path(uri: &str) -> Option<String> {
    let raw = uri
        .strip_prefix("file:///")
        .map(|path| format!("/{path}"))
        .or_else(|| {
            uri.strip_prefix("file://localhost/")
                .map(|path| format!("/{path}"))
        })?;
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let pair = bytes.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    (!path.contains('\0')).then_some(path)
}
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
pub fn command(workspace: &Workspace, argv: &[String]) -> Vec<String> {
    let mut out = workspace.prefix.clone();
    let args = argv
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            if index == 0 {
                if let Some(path) = arg.strip_prefix("~/") {
                    return format!("\"$HOME\"/{}", quote(path));
                }
            }
            quote(arg)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$PATH\"; export PATH; cd {} && exec {args}",
        quote(&workspace.root.to_string_lossy())
    );
    out.push(format!("sh -lc {}", quote(&script)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_and_uris_keep_hosts_paths_and_unicode_distinct() {
        let path = "/C:/folder with space/你好\\file.rs";
        let key = document_key("user@example.invalid:2222", path);
        assert_eq!(
            split_key(&key),
            Some(("user@example.invalid:2222".into(), path.into()))
        );
        assert_ne!(key, document_key("another.example.invalid", path));
        assert_ne!(key, PathBuf::from(path));
        assert_eq!(uri_path(&file_uri(path)), Some(path.into()));
        assert!(uri_path("https://example.invalid/a").is_none());
    }
    #[test]
    fn transport_quotes_the_root_and_every_argument() {
        let workspace = Workspace {
            root: "/project with ' quote".into(),
            prefix: vec!["ssh".into(), "--".into(), "example.invalid".into()],
            servers: HashMap::new(),
        };
        let command = command(
            &workspace,
            &[
                "~/bin/server".into(),
                "--stdio".into(),
                "literal; argument".into(),
            ],
        );
        assert_eq!(&command[..3], workspace.prefix.as_slice());
        assert!(command.last().unwrap().contains("literal; argument"));
        assert!(command.last().unwrap().contains("$HOME"));
    }
}
