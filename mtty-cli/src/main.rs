//! `mtty-cli` — control CLI (cross-platform MTP client).
//!
//! Talks to the running mtty host over the MTP socket. Examples:
//!   mtty-cli ping
//!   mtty-cli pane list
//!   mtty-cli state miao --state processing [--pane ID]
//!   mtty-cli state list
//!   mtty-cli history add --command "ls" [--cwd DIR] [--pane ID]
//!   mtty-cli history list [--pane ID]
//!   mtty-cli pane run --pane ID --data "echo hi"
//!   mtty-cli pane focus|close --pane ID

use std::path::PathBuf;

use serde_json::{json, Value};

fn usage_text() -> &'static str {
    "usage: mtty-cli [--socket PATH|tcp://host:port] [--wait SECS] <command>\n\
     commands: ping | health | wait [--since N] | events [--topic T[,T]] | \
     pane list|run|send|focus|close|output | \
     state <agent> --state S [--session ID] [--cwd DIR] [--quota FILE|-] [--runtime-context FILE|-] [--footer JSON] | state list | \
     agent sessions | agent resume [--pane ID | --session ID] | history add|list |\n     view|edit <path> |\n     file read --path P [--offset N] [--length N] [--base64] |\n     file write --path P [--data D | --data-b64 B]"
}

/// Read a JSON value from `FILE` or `-` (stdin) for state metadata.
fn read_json_arg(spec: &str) -> Option<Value> {
    let text = if spec == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).ok()?;
        buf
    } else {
        std::fs::read_to_string(spec).ok()?
    };
    serde_json::from_str(&text).ok()
}

fn usage() -> ! {
    eprintln!("{}", usage_text());
    std::process::exit(2);
}

/// Read `--flag value` pairs starting at `args[start..]`.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // `MTTY_SOCKET` (or the former `MIAOTTY_SOCKET`), else the default path —
    // or an older host's socket when only that one exists (ADR 0032).
    let mut socket = mtty_mtp::env("SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(mtty_mtp::client_socket);
    if let Some(i) = args.iter().position(|a| a == "--socket") {
        if i + 1 < args.len() {
            socket = PathBuf::from(args.remove(i + 1));
            args.remove(i);
        }
    }

    // `--wait SECS`: keep trying while mtty restarts (an update keeps the
    // pane's programs running, ADR 0041). Inside a pane the default is 5 s,
    // so agent hooks reach the relaunched app; outside, failing is immediate.
    let mut wait = std::time::Duration::from_secs(if mtty_mtp::env("PANE_ID").is_some() {
        5
    } else {
        0
    });
    if let Some(i) = args.iter().position(|a| a == "--wait") {
        if let Some(secs) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
            wait = std::time::Duration::from_secs_f64(secs.max(0.0));
            args.remove(i + 1);
        }
        args.remove(i);
    }

    let pane_default = || mtty_mtp::env("PANE_ID");
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if cmd == "--help" || cmd == "-h" {
        println!("{}", usage_text());
        return;
    }

    let deadline = std::time::Instant::now() + wait;
    let mut client = loop {
        match mtty_mtp::client::connect_any(&socket.to_string_lossy()) {
            Ok(c) => break c,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("mtty-cli: cannot connect to {}: {e}", socket.display());
                std::process::exit(1);
            }
        }
    };

    if cmd == "events" {
        // Stream events until interrupted; one JSON object per line.
        let topics: Vec<&str> = flag(&args, "--topic")
            .map(|t| t.split(',').collect())
            .unwrap_or_default();
        if let Err(e) = client.subscribe(&topics) {
            eprintln!("mtty-cli: subscribe failed: {e}");
            std::process::exit(1);
        }
        loop {
            match client.next_event() {
                Ok(Some(event)) => println!("{event}"),
                Ok(None) => break,
                Err(e) => {
                    eprintln!("mtty-cli: stream ended: {e}");
                    std::process::exit(1);
                }
            }
        }
        std::process::exit(0);
    }

    let result: std::io::Result<Value> = match (cmd, args.get(1).map(String::as_str)) {
        ("ping", _) => client.call("core", "ping", json!({})),
        ("health", _) => client.call("core", "health", json!({})),
        ("wait", _) => {
            // Block until the host's revision moves past `--since` (default 0).
            let since = flag(&args, "--since")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let timeout_ms = flag(&args, "--timeout")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(25_000);
            client.call(
                "core",
                "wait",
                json!({ "since": since, "timeout_ms": timeout_ms }),
            )
        }
        ("pane", Some("list")) => client.call("pane", "list", json!({})),
        ("pane", Some(m @ ("run" | "send" | "focus" | "close" | "output"))) => {
            let pane = flag(&args, "--pane")
                .map(str::to_string)
                .or_else(pane_default);
            if m == "focus" || m == "close" || m == "output" {
                client.call("pane", m, json!({ "pane_id": pane }))
            } else {
                let data = flag(&args, "--data").unwrap_or("");
                client.call("pane", m, json!({ "pane_id": pane, "data": data }))
            }
        }
        ("view", Some(path)) | ("edit", Some(path)) => {
            client.call("app", cmd, json!({ "path": path }))
        }
        ("file", Some(m @ ("read" | "write"))) => {
            let path = flag(&args, "--path").unwrap_or("");
            if m == "read" {
                let mut params = json!({ "path": path });
                if let Some(offset) = flag(&args, "--offset").and_then(|v| v.parse::<u64>().ok()) {
                    params["offset"] = json!(offset);
                }
                if let Some(length) = flag(&args, "--length").and_then(|v| v.parse::<u64>().ok()) {
                    params["length"] = json!(length);
                }
                if args.iter().any(|a| a == "--base64") {
                    params["encoding"] = json!("base64");
                }
                client.call("file", "read", params)
            } else if let Some(b64) = flag(&args, "--data-b64") {
                client.call("file", "write", json!({ "path": path, "data_b64": b64 }))
            } else {
                let data = flag(&args, "--data").unwrap_or("");
                client.call("file", "write", json!({ "path": path, "data": data }))
            }
        }
        ("state", Some("list")) => client.call("agent", "state.list", json!({})),
        ("state", Some(agent)) => {
            let state = flag(&args, "--state").unwrap_or("");
            let pane = flag(&args, "--pane")
                .map(str::to_string)
                .or_else(pane_default);
            let session = flag(&args, "--session");
            let cwd = flag(&args, "--cwd");
            let quota = flag(&args, "--quota").and_then(read_json_arg);
            let mut params = json!({
                "agent": agent,
                "state": state,
                "pane_id": pane,
                "session_id": session,
            });
            if let Some(cwd) = cwd {
                params["cwd"] = json!(cwd);
            }
            if let Some(context) = flag(&args, "--runtime-context").and_then(read_json_arg) {
                params["runtime_context"] = context;
            }
            if let Some(quota) = quota {
                params["quota"] = quota;
            }
            if let Some(footer) =
                flag(&args, "--footer").and_then(|text| serde_json::from_str::<Value>(text).ok())
            {
                // The agent's merged-footer facts: the host renders them into
                // its own status row and synthesizes presses back. Inline JSON
                // on the argv, so no temp file and no shell quoting; an older
                // host stores it and simply never renders it.
                params["footer"] = footer;
            }
            client.call("agent", "state.set", params)
        }
        ("agent", Some("sessions")) => client.call("agent", "sessions", json!({})),
        ("agent", Some("resume")) => {
            let pane = flag(&args, "--pane")
                .map(str::to_string)
                .or_else(pane_default);
            let session = flag(&args, "--session")
                .map(str::to_string)
                .or_else(|| args.get(2).filter(|a| !a.starts_with("--")).cloned());
            client.call(
                "agent",
                "resume",
                json!({ "pane_id": pane, "session_id": session }),
            )
        }
        ("history", Some("list")) => {
            let pane = flag(&args, "--pane")
                .map(str::to_string)
                .or_else(pane_default);
            client.call("history", "list", json!({ "pane_id": pane }))
        }
        ("history", Some("add")) => {
            let command = flag(&args, "--command").unwrap_or("");
            let cwd = flag(&args, "--cwd");
            let pane = flag(&args, "--pane")
                .map(str::to_string)
                .or_else(pane_default);
            client.call(
                "history",
                "add",
                json!({ "command": command, "cwd": cwd, "pane_id": pane }),
            )
        }
        _ => usage(),
    };

    match result {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into())
        ),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
