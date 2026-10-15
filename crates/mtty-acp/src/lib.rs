//! A minimal Agent Client Protocol (ACP) client (ADR 0040, M7 A2).
//!
//! ACP is JSON-RPC 2.0 over the agent subprocess's stdio, one message per
//! newline. This crate launches an agent, initializes and authenticates it,
//! creates or loads sessions, prompts and cancels turns, answers the agent's
//! `fs/*`, `terminal/*` and `session/request_permission` requests, and forwards `session/update`
//! notifications and responses to a [`Handler`]. It has no GPU or window host
//! dependency, so it is unit-tested against a fake agent.

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use serde_json::{json, Value};
mod settings;
mod terminal;
pub use settings::{session_settings, Setting, SettingKind};
use terminal::Terminals;

/// What the client does when the agent talks to it.
pub trait Handler: Send + Sync + 'static {
    /// A `session/update` notification's `update` object.
    fn on_update(&self, session: &str, update: &Value) {
        let _ = (session, update);
    }
    /// A response to one of our requests.
    fn on_response(&self, id: u64, result: &Value, error: Option<&Value>) {
        let _ = (id, result, error);
    }
    /// The transport failed or the agent sent unparsable data.
    fn on_error(&self, message: &str) {
        let _ = message;
    }
    /// Bounded adapter stderr, separate from protocol messages.
    fn on_log(&self, message: &str) {
        let _ = message;
    }
    /// The transport cannot accept further requests.
    fn on_closed(&self, message: &str) {
        self.on_error(message);
    }
    /// Live output from a client-managed command. Clients may keep this
    /// output for tool-call terminal references even after terminal release.
    fn on_terminal_output(&self, session: &str, terminal: &str, output: &str) {
        let _ = (session, terminal, output);
    }
    /// `fs/read_text_file`: the file's text, or `None` to refuse.
    fn read_text_file(&self, path: &str) -> Option<String> {
        let _ = path;
        None
    }
    /// `fs/write_text_file`.
    fn write_text_file(&self, path: &str, content: &str) -> bool {
        let _ = (path, content);
        false
    }
    /// `session/request_permission`: whether to allow the requested action.
    fn request_permission(&self, request: &Value) -> bool {
        let _ = request;
        false
    }
}

/// A connection to one ACP agent.
pub struct Client {
    out: mpsc::Sender<String>,
    next_id: AtomicU64,
    handler: Arc<dyn Handler>,
    child: Option<Child>,
    terminals: Arc<Terminals>,
}

impl Client {
    /// Launch `command` (with `args`/`env`, in `cwd`) and speak ACP to it.
    pub fn spawn(
        command: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        handler: Arc<dyn Handler>,
    ) -> std::io::Result<Client> {
        let mut cmd = mtty_platform::background_command(command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        if let Some(stderr) = child.stderr.take() {
            let logs = handler.clone();
            std::thread::spawn(move || read_stderr(stderr, logs));
        }
        let mut client = Client::over(stdout, stdin, handler);
        client.child = Some(child);
        Ok(client)
    }

    /// Speak ACP over any reader/writer pair (tests, or a remote transport).
    pub fn over<R, W>(reader: R, mut writer: W, handler: Arc<dyn Handler>) -> Client
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (out, rx) = mpsc::channel::<String>();
        let writer_handler = handler.clone();
        // The writer thread owns the agent's stdin.
        std::thread::spawn(move || {
            for line in rx {
                if let Err(error) = writer
                    .write_all(line.as_bytes())
                    .and_then(|_| writer.write_all(b"\n"))
                    .and_then(|_| writer.flush())
                {
                    writer_handler.on_closed(&format!("write failed: {error}"));
                    break;
                }
            }
        });
        let reply = Writer(out.clone());
        let h = handler.clone();
        let terminals = Arc::new(Terminals::default());
        let t = terminals.clone();
        std::thread::spawn(move || reader_loop(Box::new(reader), h, reply, t));
        Client {
            out,
            next_id: AtomicU64::new(1),
            handler,
            child: None,
            terminals,
        }
    }

    /// Send a request and return its id (the response arrives at
    /// [`Handler::on_response`]).
    pub fn request(&self, method: &str, params: Value) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    /// Send a notification (no response).
    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Negotiate the protocol version and capabilities.
    pub fn initialize(&self, name: &str, version: &str) -> u64 {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": self.terminals.enabled(),
                },
                "clientInfo": { "name": name, "version": version },
            }),
        )
    }

    /// Start a session rooted at `cwd` (an absolute path).
    pub fn new_session(&self, cwd: &str) -> u64 {
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
    }

    /// Enable client-managed commands before initialization. Every command
    /// requires the handler's permission, and defaults to this directory.
    pub fn enable_terminals(&self, cwd: &std::path::Path) {
        self.terminals.enable(cwd);
    }

    /// Authenticate using an id advertised in the agent's `authMethods`.
    pub fn authenticate(&self, method_id: &str) -> u64 {
        self.request("authenticate", json!({ "methodId": method_id }))
    }

    /// Resume a session after checking the agent's `loadSession` capability.
    pub fn load_session(&self, session: &str, cwd: &str) -> u64 {
        self.request(
            "session/load",
            json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] }),
        )
    }

    /// Send a prompt to a session.
    pub fn prompt(&self, session: &str, text: &str) -> u64 {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": text }],
            }),
        )
    }

    /// Ask the agent to stop the current turn.
    pub fn cancel(&self, session: &str) {
        self.terminals.cancel(session);
        self.notify("session/cancel", json!({ "sessionId": session }));
    }

    /// Change only a selector actually advertised by the agent.
    pub fn set_setting(&self, session: &str, setting: &Setting, value: &str) -> u64 {
        match setting.kind {
            SettingKind::Config => self.request(
                "session/set_config_option",
                json!({"sessionId":session,"configId":setting.id,"value":value}),
            ),
            SettingKind::Mode => self.request(
                "session/set_mode",
                json!({"sessionId":session,"modeId":value}),
            ),
        }
    }

    pub fn shutdown(&mut self) -> Option<std::process::ExitStatus> {
        self.terminals.shutdown();
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            #[cfg(windows)]
            if child.try_wait().ok().flatten().is_none() {
                let _ = mtty_platform::background_command("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            return child.wait().ok();
        }
        None
    }

    pub fn handler(&self) -> &Arc<dyn Handler> {
        &self.handler
    }

    fn send(&self, message: Value) {
        // serde_json never emits a bare newline, so a message is one line.
        let _ = self.out.send(message.to_string());
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn read_stderr(mut reader: impl Read, handler: Arc<dyn Handler>) {
    fn flush(buffer: &mut Vec<u8>, handler: &dyn Handler) {
        loop {
            match std::str::from_utf8(buffer) {
                Ok(text) => {
                    if !text.is_empty() {
                        handler.on_log(text);
                    }
                    buffer.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        handler.on_log(std::str::from_utf8(&buffer[..valid]).unwrap());
                        buffer.drain(..valid);
                        continue;
                    }
                    if let Some(length) = error.error_len() {
                        handler.on_log("\u{fffd}");
                        buffer.drain(..length);
                    } else {
                        break;
                    }
                }
            }
        }
    }
    let mut bytes = [0; 4096];
    let mut pending = Vec::new();
    let mut remaining = 64 * 1024;
    let mut truncated = false;
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => {
                flush(&mut pending, handler.as_ref());
                if !pending.is_empty() {
                    handler.on_log(&String::from_utf8_lossy(&pending));
                }
                break;
            }
            Ok(count) => {
                let take = count.min(remaining);
                pending.extend_from_slice(&bytes[..take]);
                remaining -= take;
                flush(&mut pending, handler.as_ref());
                if take < count && !truncated {
                    handler.on_log("\n[stderr capture truncated after 64 KiB]\n");
                    pending.clear();
                    truncated = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                handler.on_log(&format!("\n[stderr read failed: {error}]\n"));
                break;
            }
        }
    }
}

#[derive(Clone)]
struct Writer(mpsc::Sender<String>);

impl Writer {
    fn raw(&self, v: Value) {
        let _ = self.0.send(v.to_string());
    }
    fn result(&self, id: &Value, result: Value) {
        self.raw(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
    fn error(&self, id: &Value, code: i64, message: &str) {
        self.raw(
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        );
    }
}

fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    handler: Arc<dyn Handler>,
    writer: Writer,
    terminals: Arc<Terminals>,
) {
    // Keep bytes until a whole line arrives: reads may split a UTF-8 codepoint.
    let mut buf = Vec::new();
    let mut bytes = [0u8; 8192];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => {
                if !buf.is_empty() {
                    match std::str::from_utf8(&buf) {
                        Ok(line) => dispatch(line.trim(), &handler, &writer, &terminals),
                        Err(_) => handler.on_error("invalid UTF-8 from the agent"),
                    }
                }
                handler.on_closed("agent closed its stdout");
                break;
            }
            Ok(n) => {
                buf.extend_from_slice(&bytes[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    match std::str::from_utf8(&line) {
                        Ok(line) if !line.trim().is_empty() => {
                            dispatch(line.trim(), &handler, &writer, &terminals)
                        }
                        Ok(_) => {}
                        Err(_) => handler.on_error("invalid UTF-8 from the agent"),
                    }
                }
            }
            Err(e) => {
                handler.on_closed(&format!("read failed: {e}"));
                break;
            }
        }
    }
    terminals.shutdown();
}

fn dispatch(line: &str, handler: &Arc<dyn Handler>, writer: &Writer, terminals: &Arc<Terminals>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        handler.on_error("invalid JSON from the agent");
        return;
    };
    let method = v.get("method").and_then(Value::as_str);
    match (method, v.get("id")) {
        // A request from the agent: answer it.
        (Some(method), Some(id)) => {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            match method {
                method @ ("fs/read_text_file" | "fs/write_text_file") => {
                    let handler = handler.clone();
                    let writer = writer.clone();
                    let id = id.clone();
                    let method = method.to_string();
                    // File writes may wait for user review. Keep receiving
                    // notifications and cancellation while the handler waits.
                    std::thread::spawn(move || {
                        answer_file(&method, &params, &handler, &writer, &id)
                    });
                }
                "session/request_permission" => {
                    let handler = handler.clone();
                    let writer = writer.clone();
                    let id = id.clone();
                    let terminals = terminals.clone();
                    std::thread::spawn(move || {
                        let decision = terminals.permission(&params, &handler);
                        // Boolean handlers select only one-time options; never
                        // silently turn a one-time answer into permanent consent.
                        let kind = if decision == Some(true) {
                            "allow_once"
                        } else {
                            "reject_once"
                        };
                        let option = params
                            .get("options")
                            .and_then(Value::as_array)
                            .and_then(|options| {
                                options
                                    .iter()
                                    .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
                            })
                            .and_then(|o| o.get("optionId").and_then(Value::as_str));
                        let outcome = match option.filter(|_| decision.is_some()) {
                            Some(option) => json!({ "outcome": "selected", "optionId": option }),
                            None => json!({ "outcome": "cancelled" }),
                        };
                        writer.result(&id, json!({ "outcome": outcome }));
                    });
                }
                method if method.starts_with("terminal/") => {
                    let handler = handler.clone();
                    let writer = writer.clone();
                    let terminals = terminals.clone();
                    let id = id.clone();
                    let method = method.to_string();
                    // wait_for_exit must not block delivery of kill/release.
                    std::thread::spawn(move || {
                        match terminals.request(&method, &params, &handler) {
                            Ok(result) => writer.result(&id, result),
                            Err(message) => writer.error(&id, -32000, &message),
                        }
                    });
                }
                _ => writer.error(id, -32601, "method not found"),
            }
        }
        // A notification: forward streamed updates.
        (Some("session/update"), None) => {
            if let Some(update) = v.pointer("/params/update") {
                let session = v
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                handler.on_update(session, update);
            }
        }
        (Some(_), None) => {}
        // A response to one of our requests.
        (None, Some(id)) => {
            if let Some(id) = id.as_u64() {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                let error = v.get("error").cloned();
                handler.on_response(id, &result, error.as_ref());
            }
        }
        (None, None) => {}
    }
}

fn answer_file(
    method: &str,
    params: &Value,
    handler: &Arc<dyn Handler>,
    writer: &Writer,
    id: &Value,
) {
    let Some(path) = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| std::path::Path::new(path).is_absolute())
    else {
        writer.error(id, -32602, "path must be an absolute string path");
        return;
    };
    match method {
        "fs/read_text_file" => {
            let integer = |name, default| match params.get(name) {
                None | Some(Value::Null) => Ok(default),
                Some(value) => value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or("line and limit must be nonnegative integers"),
            };
            let range = integer("line", 1).and_then(|start| {
                if start == 0 {
                    Err("line must be at least 1")
                } else {
                    integer("limit", usize::MAX).map(|limit| (start, limit))
                }
            });
            let (start, limit) = match range {
                Ok(range) => range,
                Err(message) => {
                    writer.error(id, -32602, message);
                    return;
                }
            };
            match handler.read_text_file(path) {
                Some(content) => {
                    let content = content
                        .split_inclusive('\n')
                        .skip(start - 1)
                        .take(limit)
                        .collect::<String>();
                    writer.result(id, json!({ "content": content }));
                }
                None => writer.error(id, -32000, "file not readable"),
            }
        }
        "fs/write_text_file" => {
            let Some(content) = params.get("content").and_then(Value::as_str) else {
                writer.error(id, -32602, "content must be a string");
                return;
            };
            if handler.write_text_file(path, content) {
                writer.result(id, json!({}));
            } else {
                writer.error(id, -32000, "file not writable");
            }
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::time::Duration;

    fn test_path() -> String {
        std::env::temp_dir()
            .join("mtty-acp-test-file")
            .to_string_lossy()
            .into_owned()
    }

    #[derive(Default)]
    struct Recorder {
        tx: std::sync::Mutex<Option<mpsc::Sender<String>>>,
    }

    impl Recorder {
        fn new() -> (Arc<Self>, mpsc::Receiver<String>) {
            let (tx, rx) = mpsc::channel();
            let rec = Arc::new(Recorder {
                tx: std::sync::Mutex::new(Some(tx)),
            });
            (rec, rx)
        }
        fn send(&self, message: String) {
            if let Some(tx) = &*self.tx.lock().unwrap() {
                let _ = tx.send(message);
            }
        }
    }

    impl Handler for Recorder {
        fn on_update(&self, session: &str, update: &Value) {
            self.send(format!(
                "update:{session}:{}",
                update
                    .get("sessionUpdate")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ));
        }
        fn on_response(&self, id: u64, result: &Value, _error: Option<&Value>) {
            self.send(format!(
                "response:{id}:{}",
                result
                    .get("protocolVersion")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ));
        }
        fn read_text_file(&self, path: &str) -> Option<String> {
            (path == test_path()).then(|| "hello".to_string())
        }
    }

    // Loopback TCP provides the same bidirectional byte stream on every OS.
    fn streams() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (agent, _) = listener.accept().unwrap();
        (client, agent)
    }

    #[test]
    fn initialize_and_updates_round_trip() {
        let (client_stream, agent_stream) = streams();
        let (rec, rx) = Recorder::new();
        let client = Client::over(client_stream.try_clone().unwrap(), client_stream, rec);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(agent_stream.try_clone().unwrap());
            let mut writer = agent_stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(v["method"], "initialize");
            let id = v["id"].clone();
            writeln!(
                writer,
                "{}",
                json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": 1 } })
            )
            .unwrap();
            writeln!(
                writer,
                "{}",
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s1",
                        "update": { "sessionUpdate": "agent_message_chunk" }
                    }
                })
            )
            .unwrap();
        });
        client.initialize("mtty", "0.0.16");
        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert!(got.iter().any(|m| m == "response:1:1"), "{got:?}");
        assert!(
            got.iter().any(|m| m == "update:s1:agent_message_chunk"),
            "{got:?}"
        );
    }

    #[test]
    fn advertised_setting_requests_and_disconnects_are_observable() {
        struct Closed(mpsc::Sender<String>);
        impl Handler for Closed {
            fn on_closed(&self, message: &str) {
                let _ = self.0.send(message.into());
            }
        }
        let (transport, agent) = streams();
        let (tx, rx) = mpsc::channel();
        let client = Client::over(
            transport.try_clone().unwrap(),
            transport,
            Arc::new(Closed(tx)),
        );
        let setting = Setting {
            id: "model".into(),
            name: "Model".into(),
            current: "first".into(),
            choices: vec![("next".into(), "Next".into())],
            kind: SettingKind::Config,
        };
        client.set_setting("session", &setting, "next");
        let mut reader = BufReader::new(agent);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "session/set_config_option");
        assert_eq!(
            request["params"],
            json!({"sessionId":"session","configId":"model","value":"next"})
        );
        drop(reader);
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn stderr_preserves_split_unicode_and_is_bounded_without_blocking_the_reader() {
        struct OneByte(std::io::Cursor<Vec<u8>>);
        impl Read for OneByte {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(&mut buffer[..1])
            }
        }
        #[derive(Default)]
        struct Logs(std::sync::Mutex<String>);
        impl Handler for Logs {
            fn on_log(&self, message: &str) {
                self.0.lock().unwrap().push_str(message);
            }
        }
        let logs = Arc::new(Logs::default());
        read_stderr(
            OneByte(std::io::Cursor::new("错误：你好 🐈".as_bytes().to_vec())),
            logs.clone(),
        );
        assert_eq!(&*logs.0.lock().unwrap(), "错误：你好 🐈");
        let logs = Arc::new(Logs::default());
        read_stderr(std::io::Cursor::new(vec![b'x'; 128 * 1024]), logs.clone());
        let text = logs.0.lock().unwrap();
        assert!(text.len() < 65 * 1024 && text.contains("truncated"));
    }

    #[test]
    fn answers_an_fs_read_request() {
        let (client_stream, agent_stream) = streams();
        let (rec, _rx) = Recorder::new();
        let _client = Client::over(client_stream.try_clone().unwrap(), client_stream, rec);
        let mut writer = agent_stream.try_clone().unwrap();
        let mut reader = BufReader::new(agent_stream);
        writeln!(
            writer,
            "{}",
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "fs/read_text_file",
                "params": { "path": test_path() }
            })
        )
        .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["content"], "hello");
    }

    #[test]
    fn authentication_loading_and_terminal_capability_are_sent() {
        let (client_stream, agent_stream) = streams();
        let (rec, _) = Recorder::new();
        let client = Client::over(client_stream.try_clone().unwrap(), client_stream, rec);
        client.enable_terminals(&std::env::temp_dir());
        client.initialize("mtty", "test");
        client.authenticate("oauth");
        client.load_session("saved", "/tmp");
        let mut reader = BufReader::new(agent_stream);
        let mut values = Vec::new();
        for _ in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            values.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(values[0]["params"]["clientCapabilities"]["terminal"], true);
        assert_eq!(values[1]["method"], "authenticate");
        assert_eq!(values[1]["params"]["methodId"], "oauth");
        assert_eq!(values[2]["method"], "session/load");
        assert_eq!(values[2]["params"]["sessionId"], "saved");
    }

    #[test]
    fn permission_outcome_contains_the_selected_option_id() {
        struct Allow;
        impl Handler for Allow {
            fn request_permission(&self, _: &Value) -> bool {
                true
            }
        }
        let handler: Arc<dyn Handler> = Arc::new(Allow);
        let (tx, rx) = mpsc::channel();
        dispatch(&json!({ "id": 7, "method": "session/request_permission", "params": { "sessionId": "s", "options": [{ "optionId": "forever", "kind": "allow_always" }, { "optionId": "once", "kind": "allow_once" }] } }).to_string(), &handler, &Writer(tx), &Arc::new(Terminals::default()));
        let reply: Value =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
        assert_eq!(
            reply["result"]["outcome"],
            json!({ "outcome": "selected", "optionId": "once" })
        );
    }

    #[test]
    fn cancelling_a_turn_answers_pending_permission() {
        struct Block {
            started: mpsc::Sender<()>,
            answer: std::sync::Mutex<mpsc::Receiver<()>>,
        }
        impl Handler for Block {
            fn request_permission(&self, _: &Value) -> bool {
                self.started.send(()).unwrap();
                self.answer.lock().unwrap().recv().unwrap();
                true
            }
        }
        let (started_tx, started_rx) = mpsc::channel();
        let (answer_tx, answer_rx) = mpsc::channel();
        let handler: Arc<dyn Handler> = Arc::new(Block {
            started: started_tx,
            answer: std::sync::Mutex::new(answer_rx),
        });
        let service = Arc::new(Terminals::default());
        let (tx, rx) = mpsc::channel();
        dispatch(&json!({ "id": 9, "method": "session/request_permission", "params": { "sessionId": "s", "options": [{ "optionId": "once", "kind": "allow_once" }] } }).to_string(), &handler, &Writer(tx), &service);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        service.cancel("s");
        let reply: Value =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        answer_tx.send(()).unwrap();
    }

    #[test]
    fn file_reads_respect_one_based_line_and_limit() {
        struct Files;
        impl Handler for Files {
            fn read_text_file(&self, _: &str) -> Option<String> {
                Some("first\nsecond\nlast".into())
            }
        }
        let handler: Arc<dyn Handler> = Arc::new(Files);
        let (tx, rx) = mpsc::channel();
        dispatch(&json!({ "id": 1, "method": "fs/read_text_file", "params": { "path": test_path(), "line": 2, "limit": 1 } }).to_string(), &handler, &Writer(tx), &Arc::new(Terminals::default()));
        let reply: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(reply["result"]["content"], "second\n");
    }

    #[test]
    fn file_review_does_not_block_streamed_updates() {
        struct Review {
            answer: std::sync::Mutex<mpsc::Receiver<()>>,
            update: mpsc::Sender<()>,
        }
        impl Handler for Review {
            fn write_text_file(&self, _: &str, _: &str) -> bool {
                self.answer.lock().unwrap().recv().unwrap();
                true
            }
            fn on_update(&self, _: &str, _: &Value) {
                self.update.send(()).unwrap();
            }
        }
        let (answer_tx, answer_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let handler: Arc<dyn Handler> = Arc::new(Review {
            answer: std::sync::Mutex::new(answer_rx),
            update: update_tx,
        });
        let input = format!(
            "{}\n{}\n",
            json!({ "id": 1, "method": "fs/write_text_file", "params": { "path": test_path(), "content": "new" } }),
            json!({ "method": "session/update", "params": { "sessionId": "s", "update": {} } })
        );
        let (out, reply) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            reader_loop(
                Box::new(std::io::Cursor::new(input.into_bytes())),
                handler,
                Writer(out),
                Arc::new(Terminals::default()),
            )
        });
        let update = update_rx.recv_timeout(Duration::from_secs(2));
        answer_tx.send(()).unwrap();
        assert!(update.is_ok());
        reader.join().unwrap();
        let result: Value =
            serde_json::from_str(&reply.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
        assert_eq!(result["result"], json!({}));
    }

    #[test]
    fn transport_preserves_utf8_split_between_reads_and_final_line() {
        struct OneByte(std::io::Cursor<Vec<u8>>);
        impl Read for OneByte {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(&mut buffer[..1])
            }
        }
        struct Text(mpsc::Sender<String>);
        impl Handler for Text {
            fn on_update(&self, _: &str, update: &Value) {
                self.0
                    .send(update["content"]["text"].as_str().unwrap().into())
                    .unwrap();
            }
        }
        let (tx, rx) = mpsc::channel();
        let handler: Arc<dyn Handler> = Arc::new(Text(tx));
        let message = json!({ "method": "session/update", "params": { "sessionId": "s", "update": { "content": { "text": "你好 🐈" } } } }).to_string();
        let (out, _) = mpsc::channel();
        reader_loop(
            Box::new(OneByte(std::io::Cursor::new(message.into_bytes()))),
            handler,
            Writer(out),
            Arc::new(Terminals::default()),
        );
        assert_eq!(rx.recv().unwrap(), "你好 🐈");
    }

    #[cfg(unix)]
    #[test]
    fn dropping_client_stops_adapter_descendants() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker =
            std::env::temp_dir().join(format!("mtty-acp-drop-{}-{stamp}", std::process::id()));
        let (recorder, _) = Recorder::new();
        let client = Client::spawn(
            "sh",
            &[
                "-c".into(),
                "while true; do printf x >> \"$MTTY_ACP_MARKER\"; sleep 0.01; done & wait".into(),
            ],
            None,
            &[(
                "MTTY_ACP_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            )],
            recorder,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        let before = std::fs::metadata(&marker).unwrap().len();
        std::thread::sleep(Duration::from_millis(100));
        let after = std::fs::metadata(&marker).unwrap().len();
        std::fs::remove_file(marker).unwrap();
        assert!(before > 0);
        assert_eq!(
            after, before,
            "adapter descendant kept running after Client Drop"
        );
    }
}
