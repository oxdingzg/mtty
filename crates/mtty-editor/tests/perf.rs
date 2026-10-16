//! Performance gate for the editor core (ADR 0034, phase E1).
//!
//! `#[ignore]`d like the other perf gates; CI runs them in release mode
//! (`cargo test --release -- --ignored`). Budgets are absolute with generous
//! headroom; `MTTY_PERF_SCALE` relaxes them on slow machines.

use std::time::Instant;

use mtty_editor::{Document, Motion, SearchQuery, Selection};

fn scale() -> f64 {
    std::env::var("MTTY_PERF_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

/// About 100 MB of source-like text.
fn big_file() -> Vec<u8> {
    let line = "    let value = compute(index, &table[offset..end]); // 中文注释 needle?\n";
    let target = 100 << 20;
    let mut out = String::with_capacity(target + line.len());
    while out.len() < target {
        out.push_str(line);
    }
    out.into_bytes()
}

fn check(name: &str, ms: f64, budget_ms: f64) {
    let budget = budget_ms * scale();
    eprintln!("{name}: {ms:.4} ms (budget {budget} ms)");
    assert!(
        ms <= budget,
        "{name} took {ms:.2} ms, budget {budget:.0} ms"
    );
}

#[test]
#[ignore = "perf gate; run `cargo test --release -- --ignored`"]
fn editing_a_100_mb_file_stays_interactive() {
    let bytes = big_file();

    let t = Instant::now();
    let mut doc = Document::from_bytes(&bytes).unwrap();
    check("open 100 MB", t.elapsed().as_secs_f64() * 1e3, 1500.0);

    // Type in the middle: every keystroke must be far below a frame.
    let middle = doc.rope().len_chars() / 2;
    doc.set_selection(Selection::cursor(middle));
    let t = Instant::now();
    for _ in 0..1000 {
        doc.type_text("x");
    }
    let per_key = t.elapsed().as_secs_f64() * 1e3 / 1000.0;
    check("keystroke in the middle", per_key, 1.0);

    let t = Instant::now();
    while doc.undo() {}
    check(
        "undo 1000 keystrokes",
        t.elapsed().as_secs_f64() * 1e3,
        50.0,
    );
    assert!(!doc.is_modified());

    // Cursor motion through the file.
    let t = Instant::now();
    for _ in 0..10_000 {
        doc.move_cursor(Motion::Down, false);
    }
    let per_move = t.elapsed().as_secs_f64() * 1e3 / 10_000.0;
    check("line down", per_move, 0.5);

    let t = Instant::now();
    let hits = mtty_editor::search::find_all(doc.rope(), &SearchQuery::literal("needle?"))
        .unwrap()
        .len();
    check(
        "find all in 100 MB",
        t.elapsed().as_secs_f64() * 1e3,
        3000.0,
    );
    assert!(hits > 1_000_000);

    let t = Instant::now();
    let out = doc.to_bytes();
    check("save 100 MB", t.elapsed().as_secs_f64() * 1e3, 1000.0);
    assert_eq!(out.len(), bytes.len());
}

#[test]
#[ignore = "perf gate; run `cargo test --release -- --ignored`"]
fn highlighting_the_largest_synchronously_parsed_file_stays_interactive() {
    use mtty_editor::syntax::SYNC_PARSE_BYTES;
    use mtty_editor::Syntax;
    let unit = "/// Doc comment with 中文.\npub fn compute(index: usize, table: &[u8]) -> Option<u8> {\n    let value = table.get(index)?; // a comment\n    Some(value.wrapping_add(1))\n}\n\n";
    // Just under the size where parsing moves to a background thread.
    let text = unit.repeat(SYNC_PARSE_BYTES / unit.len() - 1);
    let mut doc = Document::from_text(&text);

    let t = Instant::now();
    let mut syntax = Syntax::for_file(std::path::Path::new("big.rs"), doc.rope()).unwrap();
    assert!(!syntax.parsing(), "parsed on this thread");
    check(
        "first parse at the synchronous size limit",
        t.elapsed().as_secs_f64() * 1e3,
        1000.0,
    );

    // One screen in the middle, as the editor pane asks every frame.
    let rope = doc.rope();
    let line = rope.len_lines() / 2;
    let range = rope.line_to_byte(line)..rope.line_to_byte(line + 60);
    let t = Instant::now();
    let mut spans = 0;
    for _ in 0..50 {
        spans += syntax.highlights(doc.rope(), range.clone()).len();
    }
    check(
        "highlight one screen",
        t.elapsed().as_secs_f64() * 1e3 / 50.0,
        4.0,
    );
    assert!(spans > 0);

    // A keystroke in the middle, inside a function body (in `value`):
    // incremental reparse.
    let at = {
        let rope = doc.rope();
        let mut l = line;
        while !rope.line(l).to_string().contains("let value") {
            l += 1;
        }
        rope.line_to_char(l) + rope.line(l).to_string().find("value").unwrap() + 2
    };
    doc.set_selection(Selection::cursor(at));
    let t = Instant::now();
    for _ in 0..20 {
        doc.type_text("x");
        let edits = doc.take_edits();
        syntax.update(doc.rope(), &edits);
    }
    check(
        "keystroke reparse",
        t.elapsed().as_secs_f64() * 1e3 / 20.0,
        16.0,
    );
}

#[test]
#[ignore = "perf gate; run `cargo test --release -- --ignored`"]
fn an_8_mb_source_file_highlights_in_the_background() {
    use mtty_editor::Syntax;
    let unit = "/// Doc comment with 中文.\npub fn compute(index: usize, table: &[u8]) -> Option<u8> {\n    let value = table.get(index)?; // a comment\n    Some(value.wrapping_add(1))\n}\n\n";
    let text = unit.repeat((8 << 20) / unit.len() - 1);
    let mut doc = Document::from_text(&text);
    let settle = |syntax: &mut Syntax| {
        let t = Instant::now();
        loop {
            syntax.poll();
            if !syntax.parsing() {
                return t.elapsed().as_secs_f64() * 1e3;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };

    let t = Instant::now();
    let mut syntax = Syntax::for_file(std::path::Path::new("big.rs"), doc.rope()).unwrap();
    check(
        "open 8 MB (UI thread)",
        t.elapsed().as_secs_f64() * 1e3,
        50.0,
    );
    check(
        "first background parse of 8 MB",
        settle(&mut syntax),
        4000.0,
    );

    // Typing: the UI thread only moves the tree; the reparse runs behind.
    let middle = doc.rope().line_to_char(doc.rope().len_lines() / 2);
    doc.set_selection(Selection::cursor(middle + 4));
    let t = Instant::now();
    for _ in 0..20 {
        doc.type_text("x");
        let edits = doc.take_edits();
        syntax.update(doc.rope(), &edits);
        syntax.poll();
    }
    check(
        "keystroke on the UI thread, 8 MB",
        t.elapsed().as_secs_f64() * 1e3 / 20.0,
        1.0,
    );
    check(
        "reparse after typing settles, 8 MB",
        settle(&mut syntax),
        2000.0,
    );
    let rope = doc.rope();
    let line = rope.len_lines() / 2;
    let range = rope.line_to_byte(line)..rope.line_to_byte(line + 60);
    assert!(!syntax.highlights(rope, range).is_empty());
}

#[test]
#[ignore = "perf gate; run `cargo test --release -- --ignored`"]
fn a_1_gb_file_opens_in_view_mode_without_loading_it() {
    use mtty_editor::large::LargeFile;
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("mtty-perf-1g-{}.log", std::process::id()));
    {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut written = 0usize;
        let mut i = 0usize;
        while written < 1 << 30 {
            let line =
                format!("2026-10-02 12:23:39,409 [INFO] request id={i:08} handled in 12ms 中文\n");
            out.write_all(line.as_bytes()).unwrap();
            written += line.len();
            i += 1;
        }
        // One match, near the end.
        out.write_all(b"the needle line\n").unwrap();
    }

    let t = Instant::now();
    let file = LargeFile::open(&path).unwrap();
    let (first, _) = file.read_lines(0, 60, usize::MAX);
    check(
        "open 1 GB and show the first screen",
        t.elapsed().as_secs_f64() * 1e3,
        100.0,
    );
    assert!(first.starts_with("2026-10-02"));

    let t = Instant::now();
    while !file.indexed() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    check(
        "index the line breaks of 1 GB",
        t.elapsed().as_secs_f64() * 1e3,
        10_000.0,
    );

    let last = file.line_count() - 2;
    let t = Instant::now();
    let (end, n) = file.read_lines(last - 59, 60, usize::MAX);
    check(
        "read the last screen of 1 GB",
        t.elapsed().as_secs_f64() * 1e3,
        50.0,
    );
    assert_eq!(n, 60);
    assert!(end.ends_with("the needle line"));

    let t = Instant::now();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut hits = Vec::new();
    file.search("NEEDLE", 10, &cancel, |a, b| hits.push((a, b)));
    check(
        "search all of 1 GB",
        t.elapsed().as_secs_f64() * 1e3,
        10_000.0,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(file.line_of(hits[0].0), last);
    let _ = std::fs::remove_file(&path);
}
