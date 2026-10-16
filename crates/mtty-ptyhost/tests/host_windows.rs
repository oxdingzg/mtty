//! The real host binary on Windows: ConPTY and AF_UNIX sockets (ADR 0041 P3).
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mtty_core::{HostConfig, HostSnapshot, Terminal};
use mtty_ptyhost::client;
use mtty_ptyhost::host::HostArgs;
use mtty_ptyhost::launch;
use mtty_ptyhost::proto::{FromHost, ToHost};
use mtty_ptyhost::sys::{self, Stream};

struct Host {
    dir: PathBuf,
    socket: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Ok((mut s, _)) = client::connect(&self.socket, Duration::from_millis(200)) {
            let _ = ToHost::Kill.write(&mut s);
        }
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn host_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mtty-ptyhost"))
}

fn start(argv: &[&str], timeout_secs: u64) -> Host {
    let dir = std::env::temp_dir().join(format!(
        "mph-{}-{}",
        std::process::id(),
        launch::new_id().unwrap().get(..8).unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("h.sock");
    let args = HostArgs {
        socket: socket.clone(),
        meta: Some(dir.join("h.json")),
        cols: 80,
        rows: 24,
        ring: 8 << 20,
        timeout: Duration::from_secs(timeout_secs),
        cwd: None,
        argv: argv.iter().map(Into::into).collect(),
    };
    let env: Vec<(String, String)> = std::env::vars().collect();
    launch::spawn(
        &host_binary(),
        &args,
        env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    )
    .unwrap();
    Host { dir, socket }
}

fn connect(host: &Host) -> (Stream, client::Welcome) {
    client::connect(&host.socket, Duration::from_secs(10)).unwrap()
}

fn read_until(s: &mut Stream, mut done: impl FnMut(&[FromHost]) -> bool) -> Vec<FromHost> {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut frames = Vec::new();
    while !done(&frames) {
        match FromHost::read(s) {
            Ok(frame) => frames.push(frame),
            Err(e) => panic!("no frame before the deadline ({e}); got {frames:?}"),
        }
    }
    frames
}

fn output(frames: &[FromHost]) -> String {
    frames
        .iter()
        .filter_map(|f| match f {
            FromHost::Output { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .collect()
}

fn printed(frames: &[FromHost], marker: &str) -> bool {
    // ConPTY can position the following prompt with CSI H rather than a
    // newline. Decode the actual screen, so the prompt does not appear joined
    // to the marker and an echoed command cannot satisfy the assertion.
    let mut screen = mtty_core::aterm::ATerm::new(80, 24, 100);
    for frame in frames {
        if let FromHost::Output { bytes, .. } = frame {
            screen.process(bytes);
        }
    }
    (0..24).any(|row| screen.line_text(row).trim() == marker)
}

fn live(frames: &[FromHost]) -> Option<u64> {
    frames.iter().find_map(|f| match f {
        FromHost::Live { offset } => Some(*offset),
        _ => None,
    })
}

fn end(frames: &[FromHost]) -> u64 {
    frames
        .iter()
        .filter_map(|f| match f {
            FromHost::Output { offset, bytes, .. } => Some(offset + bytes.len() as u64),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn gone(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[test]
fn cmd_keeps_running_and_resumes_from_an_offset() {
    let host = start(&["cmd.exe", "/Q", "/K", "echo hello-host"], 60);
    let (mut s, welcome) = connect(&host);
    assert!(welcome.child_pid > 0);
    ToHost::Attach { from: 0 }.write(&mut s).unwrap();
    let frames = read_until(&mut s, |f| {
        output(f).contains("hello-host") && live(f).is_some()
    });
    let mut seen = end(&frames);
    ToHost::Input(b"echo typed-one\r".to_vec())
        .write(&mut s)
        .unwrap();
    let more = read_until(&mut s, |f| printed(f, "typed-one"));
    seen = seen.max(end(&more));
    ToHost::Detach.write(&mut s).unwrap();
    drop(s);

    let (mut s, _) = connect(&host);
    ToHost::Attach { from: seen }.write(&mut s).unwrap();
    let frames = read_until(&mut s, |f| live(f).is_some());
    assert!(
        !output(&frames).contains("hello-host"),
        "only what came after: {:?}",
        output(&frames)
    );
    ToHost::Input(b"echo typed-two\r".to_vec())
        .write(&mut s)
        .unwrap();
    read_until(&mut s, |f| printed(f, "typed-two"));
    ToHost::Kill.write(&mut s).unwrap();
    assert!(gone(&host.socket), "the host ends after Kill");
}

#[test]
fn ctrl_c_reaches_the_program() {
    let host = start(&["cmd.exe", "/Q", "/K"], 60);
    let (mut s, _) = connect(&host);
    ToHost::Attach { from: 0 }.write(&mut s).unwrap();
    read_until(&mut s, |f| live(f).is_some());
    ToHost::Input(b"ping -n 30 127.0.0.1\r".to_vec())
        .write(&mut s)
        .unwrap();
    // Two replies in, interrupt it and require a following shell command to
    // finish promptly. The ping summary and Ctrl+C text depend on the locale.
    read_until(&mut s, |f| output(f).matches("127.0.0.1").count() >= 3);
    ToHost::Input(vec![3]).write(&mut s).unwrap();
    ToHost::Input(b"echo interrupted-host\r".to_vec())
        .write(&mut s)
        .unwrap();
    read_until(&mut s, |f| printed(f, "interrupted-host"));
}

#[test]
fn an_exit_while_detached_keeps_its_status() {
    let host = start(&["cmd.exe", "/C", "echo bye-host & exit /b 3"], 60);
    std::thread::sleep(Duration::from_millis(1500));
    let (mut s, _) = connect(&host);
    ToHost::Attach { from: 0 }.write(&mut s).unwrap();
    let frames = read_until(&mut s, |f| {
        f.iter().any(|f| matches!(f, FromHost::Exited { .. }))
    });
    assert!(
        output(&frames).contains("bye-host"),
        "{:?}",
        output(&frames)
    );
    assert!(frames
        .iter()
        .any(|f| matches!(f, FromHost::Exited { status: 3, .. })));
    drop(s);
    assert!(gone(&host.socket));
}

#[test]
fn nobody_returning_ends_the_program_tree() {
    let host = start(&["cmd.exe", "/C", "ping -n 300 127.0.0.1 >nul"], 1);
    let (mut s, welcome) = connect(&host);
    ToHost::Attach { from: 0 }.write(&mut s).unwrap();
    read_until(&mut s, |f| live(f).is_some());
    drop(s);
    assert!(gone(&host.socket), "ends after the detach timeout");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !sys::alive(welcome.child_pid.into()),
        "the program was ended"
    );
}

#[test]
fn a_second_client_takes_over() {
    let host = start(&["cmd.exe", "/Q", "/K"], 60);
    let (mut a, _) = connect(&host);
    ToHost::Attach { from: 0 }.write(&mut a).unwrap();
    read_until(&mut a, |f| live(f).is_some());
    let (mut b, _) = connect(&host);
    ToHost::Attach { from: 0 }.write(&mut b).unwrap();
    read_until(&mut b, |f| live(f).is_some());
    read_until(&mut a, |f| {
        f.iter().any(|f| matches!(f, FromHost::Detached))
    });
    ToHost::Input(b"echo from-b\r".to_vec())
        .write(&mut b)
        .unwrap();
    read_until(&mut b, |f| output(f).contains("from-b"));
}

fn screen_text(term: &Terminal) -> String {
    let (rows, _) = term.size();
    (0..rows)
        .map(|r| term.screen().line_text(r))
        .collect::<Vec<_>>()
        .join("\n")
}

fn pump_until(term: &mut Terminal, what: &str, done: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        term.process_pending();
        let text = screen_text(term);
        if done(&text) {
            return;
        }
        assert!(Instant::now() < deadline, "waiting for {what}:\n{text}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_hosted_terminal_survives_the_app() {
    let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    let config = HostConfig {
        binary: host_binary(),
        ring: 8 << 20,
        timeout: Duration::from_secs(60),
    };
    let mut term = Terminal::new_hosted(
        &config,
        Some("cmd.exe".into()),
        // Wide enough that the long prompt never wraps a command.
        200,
        24,
        1000,
        Some(std::env::temp_dir()),
        &[("PROMPT".into(), "mtty-test-ready> ".into())],
        waker.clone(),
    )
    .expect("host started");
    assert!(term.is_hosted());
    // Cmd can flush startup console input. This test checks detach/reattach,
    // so begin the interaction after its first prompt rather than racing init.
    pump_until(&mut term, "the initial cmd prompt", |text| {
        text.contains("mtty-test-ready>")
    });
    // The long prompt wraps the echoed command; its output is a line alone.
    let printed = |word: &'static str| move |t: &str| t.lines().any(|l| l.trim() == word);
    term.write(b"echo one-here\r");
    pump_until(&mut term, "one-here", printed("one-here"));
    // Output made while the app is away.
    term.write(b"ping -n 3 127.0.0.1 >nul & echo two-here\r");
    pump_until(&mut term, "the command", |t| t.contains("ping -n 3"));
    let snapshot: HostSnapshot = term.host_snapshot(1000).unwrap_or_else(|| {
        panic!(
            "a snapshot (hosted={}):\n{}",
            term.is_hosted(),
            screen_text(&term)
        )
    });
    let (id, socket) = {
        let (id, socket) = term.host_id().unwrap();
        (id.to_string(), socket.to_path_buf())
    };
    term.detach_host();
    drop(term);
    std::thread::sleep(Duration::from_secs(4));

    let mut term =
        Terminal::reattach(&id, &socket, Some(snapshot), 200, 24, 1000, waker).expect("reattached");
    pump_until(&mut term, "two-here", printed("two-here"));
    let text = screen_text(&term);
    assert_eq!(
        text.lines().filter(|l| l.trim() == "one-here").count(),
        1,
        "nothing doubled:\n{text}"
    );
    term.write(b"echo three-here\r");
    pump_until(&mut term, "three-here", printed("three-here"));
    drop(term);
    assert!(gone(&socket), "closing the pane ends the host");
}
