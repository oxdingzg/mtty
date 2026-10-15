//! Against real servers (ADR 0034, E5 acceptance): rust-analyzer and
//! typescript-language-server. Ignored by default, as CI has neither; run
//! with `cargo test -p mtty-lsp -- --ignored` where they are installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mtty_editor::Rope;
use mtty_lsp::{pos_to_char, Event, Lsp, Settings};

fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mtty-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    dir
}

/// Poll until `want` picks an event, or fail after `secs`.
fn wait<T>(
    lsp: &mut Lsp,
    secs: u64,
    what: &str,
    mut want: impl FnMut(&mut Lsp, Event) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for event in lsp.poll() {
            if let Event::Failed { message, .. } = &event {
                panic!("server failed: {message}");
            }
            if let Some(found) = want(lsp, event) {
                return found;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn installed(program: &str) -> bool {
    mtty_lsp::env::which(program, &mtty_lsp::env::search_path()).is_some()
}

/// The char just after the first `needle` (or at its start).
fn at(text: &str, needle: &str, after: bool) -> usize {
    let byte = text.find(needle).unwrap() + if after { needle.len() } else { 0 };
    text[..byte].chars().count()
}

/// One server's checks: the file, its text, where a call and a member
/// access are, what they should give, and the text with the error fixed.
struct Case<'a> {
    file: &'a Path,
    language: &'a str,
    text: &'a str,
    call: usize,
    member_at: usize,
    expect_member: &'a str,
    expect_hover: &'a str,
    fixed: &'a str,
}

fn exercise(lsp: &mut Lsp, case: Case<'_>) {
    let Case {
        file,
        language,
        text,
        call,
        member_at,
        expect_member,
        expect_hover,
        fixed,
    } = case;
    let rope = Rope::from_str(text);
    lsp.sync(file, Some(language), &rope, 0);
    assert!(lsp.handles(file));
    let started = Instant::now();
    // Diagnostics: the deliberate type error.
    wait(lsp, 120, "diagnostics", |lsp, e| match e {
        Event::Diagnostics(p) if p == file && !lsp.diagnostics(file).is_empty() => Some(()),
        _ => None,
    });
    let d = &lsp.diagnostics(file)[0];
    eprintln!(
        "{language}: diagnostics after {:.1} s: {:?} {}",
        started.elapsed().as_secs_f64(),
        d.range,
        d.message
    );
    assert_eq!(d.severity, 1);
    // Hover on the call.
    assert!(lsp.hover(file, &rope, call));
    let markdown = wait(lsp, 30, "hover", |_, e| match e {
        Event::Hover { markdown, .. } => Some(markdown),
        _ => None,
    });
    assert!(markdown.contains(expect_hover), "{markdown}");
    // Definition of the call: the function's line (0).
    assert!(lsp.definition(file, &rope, call));
    let (targets, encoding) = wait(lsp, 30, "definition", |_, e| match e {
        Event::Definition {
            targets, encoding, ..
        } => Some((targets, encoding)),
        _ => None,
    });
    assert_eq!(
        targets[0].0.canonicalize().unwrap(),
        file.canonicalize().unwrap()
    );
    let def = pos_to_char(&rope, targets[0].1.start, encoding);
    assert_eq!(rope.char_to_line(def), 0);
    // Completion of a member.
    let ticket = lsp.completion(file, &rope, member_at, Some(".")).unwrap();
    let items = wait(lsp, 30, "completion", |_, e| match e {
        Event::Completion {
            ticket: t, items, ..
        } if t == ticket => Some(items),
        _ => None,
    });
    assert!(
        items.iter().any(|i| i.label.starts_with(expect_member)),
        "{:?}",
        items.iter().map(|i| &i.label).take(20).collect::<Vec<_>>()
    );
    eprintln!("{language}: {} completions", items.len());
    // An edit goes to the server: completing the member clears the
    // syntax error.
    let syntax_error = lsp.diagnostics(file)[0].message.clone();
    let fixed_rope = Rope::from_str(fixed);
    lsp.sync(file, Some(language), &fixed_rope, 1);
    let started = Instant::now();
    wait(lsp, 60, "diagnostics after the edit", |lsp, e| match e {
        Event::Diagnostics(p)
            if p == file
                && lsp
                    .diagnostics(file)
                    .iter()
                    .all(|d| d.message != syntax_error) =>
        {
            Some(())
        }
        _ => None,
    });
    eprintln!(
        "{language}: re-checked after an edit in {:.2} s, {} diagnostics left",
        started.elapsed().as_secs_f64(),
        lsp.diagnostics(file).len()
    );
}

#[test]
#[ignore = "needs rust-analyzer"]
fn rust_analyzer() {
    if !installed("rust-analyzer") {
        panic!("rust-analyzer is not installed");
    }
    // A type of its own: std's members need the rust-src component.
    let text = "fn helper(x: i32) -> Counter {\n    Counter { n: x }\n}\n\nstruct Counter {\n    n: i32,\n}\n\nimpl Counter {\n    fn bump(&self) -> i32 {\n        self.n + 1\n    }\n}\n\nfn main() {\n    let value = helper(1);\n    let s: String = 5;\n    value.\n}\n";
    let dir = project(
        "ra",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/main.rs", text),
        ],
    );
    let file = dir.join("src/main.rs");
    let mut lsp = Lsp::new(Settings::default(), Arc::new(|| {}));
    exercise(
        &mut lsp,
        Case {
            file: &file,
            language: "Rust",
            text,
            call: at(text, "helper(1)", false) + 2,
            member_at: at(text, "value.", true),
            expect_member: "bump",
            expect_hover: "fn helper",
            fixed: &text.replace("    value.\n", "    value.bump();\n"),
        },
    );
    lsp.shutdown();
}

#[test]
#[ignore = "needs typescript-language-server"]
fn typescript_language_server() {
    if !installed("typescript-language-server") {
        panic!("typescript-language-server is not installed");
    }
    let text = "function greet(name: string): string {\n  return \"hi \" + name;\n}\nconst n: number = \"x\";\ngreet(\"a\").\n";
    let dir = project(
        "ts",
        &[
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"strict\": true } }\n",
            ),
            ("src/a.ts", text),
        ],
    );
    let file = dir.join("src/a.ts");
    let mut lsp = Lsp::new(Settings::default(), Arc::new(|| {}));
    exercise(
        &mut lsp,
        Case {
            file: &file,
            language: "TypeScript",
            text,
            call: at(text, "greet(\"a\")", false) + 1,
            member_at: at(text, "greet(\"a\").", true),
            expect_member: "toUpperCase",
            expect_hover: "greet",
            fixed: &text.replace("greet(\"a\").\n", "greet(\"a\").length;\n"),
        },
    );
    lsp.shutdown();
}
