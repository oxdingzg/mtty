//! A hosted `Terminal` across a simulated app restart (ADR 0041 P1).
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mtty_core::{HostConfig, HostSnapshot, Terminal};

fn config(binary: PathBuf) -> HostConfig {
    HostConfig {
        binary,
        ring: 8 << 20,
        timeout: Duration::from_secs(60),
    }
}

fn host_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mtty-ptyhost"))
}

fn waker() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| {})
}

fn screen_text(term: &Terminal) -> String {
    let (rows, _) = term.size();
    (0..rows)
        .map(|r| term.screen().line_text(r))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pump output until `done` holds for the screen (5 s at most).
fn pump_until(term: &mut Terminal, what: &str, done: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        term.process_pending();
        let text = screen_text(term);
        if done(&text) {
            return;
        }
        assert!(Instant::now() < deadline, "waiting for {what}:\n{text}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn hosted_shell() -> Terminal {
    let env = [("PS1".to_string(), "$ ".to_string())];
    let mut term = Terminal::new_hosted(
        &config(host_binary()),
        Some("/bin/sh".into()),
        60,
        12,
        1000,
        Some(std::env::temp_dir()),
        &env,
        waker(),
    )
    .expect("host started");
    // Wait for the shell's initial prompt before typing. Otherwise startup
    // output can place the prompt before the first command's result.
    pump_until(&mut term, "shell prompt", |text| {
        text.lines().any(|line| line.trim_end() == "$")
    });
    term
}

#[test]
fn osc_title_survives_snapshot_reattach_and_accepts_fresh_titles() {
    let mut term = hosted_shell();
    term.write(b"printf '\\033]2;miao | saved session\\007'; echo title-ready\r");
    pump_until(&mut term, "title ready", |text| {
        text.contains("\ntitle-ready")
    });
    assert_eq!(term.title(), Some("miao | saved session"));
    let snapshot = term.host_snapshot(1000).expect("live snapshot");
    let saved = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(saved["title"], "miao | saved session");
    let mut legacy = saved.clone();
    legacy.as_object_mut().unwrap().remove("title");
    assert_eq!(
        serde_json::from_value::<HostSnapshot>(legacy)
            .unwrap()
            .title,
        None
    );
    let id = snapshot.id.clone();
    let socket = snapshot.socket.clone();
    term.detach_host();
    drop(term);

    let snapshot = serde_json::from_value(saved).unwrap();
    let mut term = Terminal::reattach(&id, &socket, Some(snapshot), 60, 12, 1000, waker()).unwrap();
    // The producer has not emitted any new OSC: offset-based replay cannot
    // recover the old title, so it must already be present from the snapshot.
    assert_eq!(term.title(), Some("miao | saved session"));
    term.write(b"echo reattached\r");
    pump_until(&mut term, "reattached", |text| {
        text.contains("\nreattached")
    });
    assert_eq!(term.title(), Some("miao | saved session"));
    term.write(b"printf '\\033]2;miao | new session\\007'; echo title-updated\r");
    pump_until(&mut term, "fresh title", |text| {
        text.contains("\ntitle-updated")
    });
    assert_eq!(term.title(), Some("miao | new session"));
}

#[test]
fn the_program_survives_the_app_and_the_screen_comes_back() {
    let mut term = hosted_shell();
    assert!(term.is_hosted());
    term.write(b"echo one\r");
    pump_until(&mut term, "one", |t| t.contains("\none"));
    let pid = term.pid().expect("the shell's pid");
    // Output the program makes while the app is away, with a query in it.
    term.write(b"sleep 1; printf '\\033[c'; echo two\r");
    pump_until(&mut term, "the command echo", |t| t.contains("echo two"));
    let snapshot: HostSnapshot = term.host_snapshot(1000).expect("a snapshot");
    let saved = serde_json::to_string(&snapshot).unwrap();
    let (id, socket) = {
        let (id, socket) = term.host_id().unwrap();
        (id.to_string(), socket.to_path_buf())
    };
    term.detach_host();
    drop(term);
    std::thread::sleep(Duration::from_millis(1800));

    let snapshot: HostSnapshot = serde_json::from_str(&saved).unwrap();
    let mut term = Terminal::reattach(&id, &socket, Some(snapshot), 60, 12, 1000, waker())
        .expect("reattached");
    pump_until(&mut term, "two", |t| t.contains("\ntwo"));
    let text = screen_text(&term);
    assert_eq!(text.matches("\none").count(), 1, "nothing doubled:\n{text}");
    assert_eq!(term.pid(), Some(pid), "the same shell");
    term.write(b"echo three\r");
    pump_until(&mut term, "three", |t| t.contains("\nthree"));
    let text = screen_text(&term);
    // Answering the replayed query again would have typed `?6…c` into the
    // shell, showing up in the next command line.
    assert!(
        !text.contains("?6"),
        "a replayed query was answered:\n{text}"
    );
    // The foreground program is known without the PTY master.
    term.write(b"sleep 5\r");
    let deadline = Instant::now() + Duration::from_secs(3);
    while term.foreground_command().as_deref() != Some("sleep") {
        assert!(
            Instant::now() < deadline,
            "foreground {:?}",
            term.foreground_command()
        );
        term.process_pending();
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(term);
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "closing the pane ends the host");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_host_that_cannot_start_falls_back() {
    assert!(Terminal::new_hosted(
        &config(PathBuf::from("/nonexistent/mtty-ptyhost")),
        Some("/bin/sh".into()),
        40,
        10,
        100,
        None,
        &[],
        waker(),
    )
    .is_err());
    // A "host" that exits at once never answers: the pane runs locally.
    let mut term = Terminal::new_hosted(
        &config(PathBuf::from("/usr/bin/false")),
        Some("/bin/sh".into()),
        60,
        10,
        100,
        None,
        &[("PS1".to_string(), "$ ".to_string())],
        waker(),
    )
    .expect("spawned");
    pump_until(&mut term, "the fallback note", |t| t.contains("[mtty]"));
    assert!(!term.is_hosted());
    // Type at the prompt: typed ahead, dash echoes the line before it.
    pump_until(&mut term, "the prompt", |t| t.lines().any(|l| l == "$"));
    term.write(b"echo local\r");
    pump_until(&mut term, "local", |t| t.contains("\nlocal"));
}

#[test]
fn reattaching_without_a_snapshot_replays_everything() {
    let mut term = hosted_shell();
    term.write(b"echo first\r");
    pump_until(&mut term, "first", |t| t.contains("\nfirst"));
    let (id, socket) = {
        let (id, socket) = term.host_id().unwrap();
        (id.to_string(), socket.to_path_buf())
    };
    term.detach_host();
    drop(term);
    let mut term = Terminal::reattach(&id, &socket, None, 60, 12, 1000, waker()).unwrap();
    pump_until(&mut term, "first", |t| t.contains("\nfirst"));
    assert!(Terminal::reattach("not-an-id", &socket, None, 60, 12, 1000, waker()).is_err());
    term.end_host();
    drop(term);
}
