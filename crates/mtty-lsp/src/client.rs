//! One language server process: started, initialized and read on
//! background threads, so the UI thread never waits on it.
//!
//! Messages sent before the server has answered `initialize` are held and
//! go out, in order, right after `initialized`.

use std::collections::VecDeque;
use std::io::{BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::position::{path_to_uri, Encoding};
use crate::rpc;

/// Wakes the UI when something arrives.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// What a server sent (or what happened to it).
#[derive(Debug)]
pub enum Incoming {
    /// The answer to request `id`: its result, or the error's message.
    Response {
        id: i64,
        result: Result<Value, String>,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// The server answered `initialize`.
    Ready,
    /// It could not be started, or it stopped; the message says why.
    Failed(String),
}

/// What the server can do, from its `initialize` answer.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub encoding: Encoding,
    pub hover: bool,
    pub completion: bool,
    pub definition: bool,
    pub trigger_characters: Vec<String>,
}

impl Capabilities {
    fn from_json(caps: &Value) -> Self {
        let has = |key: &str| {
            caps.get(key)
                .is_some_and(|v| !v.is_null() && v != &json!(false))
        };
        Capabilities {
            encoding: Encoding::from_name(caps.get("positionEncoding").and_then(|v| v.as_str())),
            hover: has("hoverProvider"),
            completion: has("completionProvider"),
            definition: has("definitionProvider"),
            trigger_characters: caps
                .pointer("/completionProvider/triggerCharacters")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

enum Out {
    Message(Value),
    Change(String),
    /// `initialize` was answered: send `initialized`, then what was held.
    Ready,
    Shutdown,
}

pub struct Client {
    out: Sender<Out>,
    incoming: Arc<Mutex<VecDeque<Incoming>>>,
    caps: Arc<Mutex<Option<Capabilities>>>,
    child: Arc<Mutex<Option<Child>>>,
    closed: Arc<AtomicBool>,
    changes: Arc<Mutex<std::collections::HashMap<String, Value>>>,
    next_id: i64,
}

impl Client {
    /// Start `command` (the program and its arguments) for the workspace at
    /// `root`. Returns at once; failure to start arrives as
    /// [`Incoming::Failed`].
    pub fn start(command: Vec<String>, root: &Path, waker: Waker) -> Client {
        Self::start_with(
            command,
            Some(root.to_path_buf()),
            initialize_request(root),
            waker,
        )
    }

    pub fn start_remote(command: Vec<String>, root: &Path, waker: Waker) -> Client {
        let mut initialize = initialize_request(root);
        let root = root.to_string_lossy();
        let uri = crate::remote::file_uri(&root);
        initialize["params"]["processId"] = Value::Null;
        initialize["params"]["rootUri"] = json!(uri);
        initialize["params"]["rootPath"] = json!(root);
        initialize["params"]["workspaceFolders"] =
            json!([{"uri":uri,"name":root.rsplit('/').next().unwrap_or("")}]);
        Self::start_with(command, None, initialize, waker)
    }

    fn start_with(
        command: Vec<String>,
        root: Option<PathBuf>,
        initialize: Value,
        waker: Waker,
    ) -> Client {
        let (out, rx) = channel();
        let incoming = Arc::new(Mutex::new(VecDeque::new()));
        let caps = Arc::new(Mutex::new(None));
        let child = Arc::new(Mutex::new(None));
        let closed = Arc::new(AtomicBool::new(false));
        let changes = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let client = Client {
            out: out.clone(),
            incoming: incoming.clone(),
            caps: caps.clone(),
            child: child.clone(),
            closed: closed.clone(),
            changes: changes.clone(),
            next_id: 1,
        };
        let _ = out.send(Out::Message(initialize));
        let _ = std::thread::Builder::new()
            .name("mtty-lsp".into())
            .spawn(move || {
                run(
                    command, root, rx, out, incoming, caps, child, waker, closed, changes,
                );
            });
        client
    }

    /// The server's capabilities, once it has answered `initialize`.
    pub fn capabilities(&self) -> Option<Capabilities> {
        self.caps.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        if method == "textDocument/didChange" {
            if let Some(uri) = params
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let message = json!({"jsonrpc":"2.0","method":method,"params":params});
                let first = self
                    .changes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(uri.clone(), message)
                    .is_none();
                if first {
                    let _ = self.out.send(Out::Change(uri));
                }
                return;
            }
        }
        let _ = self.out.send(Out::Message(
            json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        ));
    }

    /// Send a request; its answer comes back from [`Client::poll`] with
    /// this id.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.out.send(Out::Message(
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        ));
        id
    }

    /// What arrived since the last call.
    pub fn poll(&mut self) -> Vec<Incoming> {
        self.incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Ask the server to exit; it is killed if still running when the
    /// client is dropped.
    pub fn shutdown(&mut self) {
        let _ = self.out.send(Out::Shutdown);
    }

    pub fn stop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.out.send(Out::Shutdown);
        if let Some(child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            std::thread::spawn(move || kill_owned(child));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.out.send(Out::Shutdown);
        if let Some(child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            kill_owned(child);
        }
    }
}

fn kill_owned(mut child: Child) {
    std::thread::sleep(std::time::Duration::from_millis(20));
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = mtty_platform::background_command("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn initialize_request(root: &Path) -> Value {
    let uri = path_to_uri(root);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "clientInfo": { "name": "mtty", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": uri,
            "rootPath": root,
            "workspaceFolders": [{ "uri": uri, "name": name }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    "publishDiagnostics": { "relatedInformation": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "documentationFormat": ["markdown", "plaintext"],
                        },
                        "contextSupport": true,
                    },
                    "definition": { "linkSupport": true },
                },
                "workspace": { "workspaceFolders": true, "configuration": true },
                "window": { "workDoneProgress": false },
            },
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn run(
    command: Vec<String>,
    root: Option<PathBuf>,
    rx: Receiver<Out>,
    out: Sender<Out>,
    incoming: Arc<Mutex<VecDeque<Incoming>>>,
    caps: Arc<Mutex<Option<Capabilities>>>,
    child_slot: Arc<Mutex<Option<Child>>>,
    waker: Waker,
    closed: Arc<AtomicBool>,
    changes: Arc<Mutex<std::collections::HashMap<String, Value>>>,
) {
    let push = |item: Incoming| {
        incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(item);
        waker();
    };
    if closed.load(Ordering::Acquire) {
        return;
    }
    let Some((program, args)) = command.split_first() else {
        push(Incoming::Failed("no command".into()));
        return;
    };
    let path = crate::env::search_path();
    let resolved = crate::env::which(program, &path).unwrap_or_else(|| PathBuf::from(program));
    let mut cmd = mtty_platform::background_command(&resolved);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(root) = &root {
        cmd.current_dir(root);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(path) = &path {
        cmd.env("PATH", path);
    }
    if closed.load(Ordering::Acquire) {
        return;
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            push(Incoming::Failed(format!("{program}: {e}")));
            return;
        }
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        push(Incoming::Failed(format!("{program}: no pipes")));
        return;
    };
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = child.stderr.take() {
        let log = stderr.clone();
        std::thread::spawn(move || {
            let mut bytes = [0; 1024];
            while let Ok(count) = pipe.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let mut text = log.lock().unwrap_or_else(|e| e.into_inner());
                text.push_str(&String::from_utf8_lossy(&bytes[..count]));
                if text.len() > 4096 {
                    let mut at = text.len() - 4096;
                    while !text.is_char_boundary(at) {
                        at += 1;
                    }
                    text.drain(..at);
                }
            }
        });
    }
    {
        let mut slot = child_slot.lock().unwrap_or_else(|e| e.into_inner());
        if closed.load(Ordering::Acquire) {
            drop(slot);
            kill_owned(child);
            return;
        }
        *slot = Some(child);
    }

    // Reader: responses and notifications to the queue; requests from the
    // server answered here.
    let reader_out = out.clone();
    let reader_waker = waker.clone();
    let reader_incoming = incoming.clone();
    let program_name = program.clone();
    let reader_closed = closed.clone();
    let _ = std::thread::Builder::new()
        .name("mtty-lsp-read".into())
        .spawn(move || {
            let push = |item: Incoming| {
                if reader_closed.load(Ordering::Acquire) {
                    return;
                }
                reader_incoming
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(item);
                reader_waker();
            };
            let mut reader = BufReader::new(stdout);
            loop {
                let msg = match rpc::read_message(&mut reader) {
                    Ok(Some(m)) => m,
                    Ok(None) => {
                        let log = stderr.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        push(Incoming::Failed(format!("{program_name} exited\n{log}")));
                        return;
                    }
                    Err(e) => {
                        push(Incoming::Failed(format!("{program_name}: {e}")));
                        return;
                    }
                };
                let method = msg.get("method").and_then(|m| m.as_str());
                let id = msg.get("id").cloned();
                match (method, id) {
                    (Some(method), Some(id)) => {
                        let reply = answer_server_request(method, msg.get("params"), id);
                        let _ = reader_out.send(Out::Message(reply));
                    }
                    (Some(method), None) => push(Incoming::Notification {
                        method: method.to_string(),
                        params: msg.get("params").cloned().unwrap_or(Value::Null),
                    }),
                    (None, Some(id)) => {
                        let Some(id) = id.as_i64() else { continue };
                        let result = match msg.get("error") {
                            Some(err) => Err(err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("error")
                                .to_string()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        if id == 0 {
                            if let Ok(result) = &result {
                                let c = Capabilities::from_json(
                                    result.get("capabilities").unwrap_or(&Value::Null),
                                );
                                *caps.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
                            }
                            let _ = reader_out.send(if result.is_ok() {
                                Out::Ready
                            } else {
                                Out::Shutdown
                            });
                            push(match result {
                                Ok(_) => Incoming::Ready,
                                Err(e) => Incoming::Failed(format!("initialize: {e}")),
                            });
                            continue;
                        }
                        push(Incoming::Response { id, result });
                    }
                    (None, None) => {}
                }
            }
        });

    // Writer: `initialize` first, the rest once it is answered.
    let mut writer = BufWriter::new(stdin);
    let mut ready = false;
    let mut held = Vec::new();
    while let Ok(item) = rx.recv() {
        if closed.load(Ordering::Acquire) && !matches!(item, Out::Shutdown) {
            continue;
        }
        let result = match item {
            Out::Message(msg) => {
                let is_init = msg.get("id") == Some(&json!(0));
                // Answers to the server's own requests may go out early.
                let is_reply = msg.get("method").is_none();
                if ready || is_init || is_reply {
                    rpc::write_message(&mut writer, &msg)
                } else {
                    held.push(Out::Message(msg));
                    Ok(())
                }
            }
            Out::Change(uri) => {
                if ready {
                    let message = changes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&uri);
                    match message {
                        Some(message) => rpc::write_message(&mut writer, &message),
                        None => Ok(()),
                    }
                } else {
                    held.push(Out::Change(uri));
                    Ok(())
                }
            }
            Out::Ready => {
                ready = true;
                let mut r = rpc::write_message(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
                );
                for item in held.drain(..) {
                    if r.is_ok() {
                        let message = match item {
                            Out::Message(message) => Some(message),
                            Out::Change(uri) => changes
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&uri),
                            _ => None,
                        };
                        if let Some(message) = message {
                            r = rpc::write_message(&mut writer, &message);
                        }
                    }
                }
                r
            }
            Out::Shutdown => {
                let _ = rpc::write_message(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": -1, "method": "shutdown" }),
                );
                let _ =
                    rpc::write_message(&mut writer, &json!({ "jsonrpc": "2.0", "method": "exit" }));
                return;
            }
        };
        if result.is_err() {
            return;
        }
    }
}

/// Answers to what servers ask of a client: settings (none), dynamic
/// registration and progress tokens (accepted), anything else (unknown).
fn answer_server_request(method: &str, params: Option<&Value>, id: Value) -> Value {
    let result = match method {
        "workspace/configuration" => {
            let n = params
                .and_then(|p| p.get("items"))
                .and_then(|i| i.as_array())
                .map_or(0, |a| a.len());
            Some(Value::Array(vec![Value::Null; n]))
        }
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "window/showMessageRequest" => Some(Value::Null),
        "workspace/workspaceFolders" => Some(Value::Null),
        _ => None,
    };
    match result {
        Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not supported") },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_read_encoding_and_triggers() {
        let caps = Capabilities::from_json(&json!({
            "positionEncoding": "utf-8",
            "hoverProvider": true,
            "completionProvider": { "triggerCharacters": [".", ":"] },
            "definitionProvider": false,
        }));
        assert_eq!(caps.encoding, Encoding::Utf8);
        assert!(caps.hover && caps.completion && !caps.definition);
        assert_eq!(caps.trigger_characters, vec![".", ":"]);
        assert_eq!(
            Capabilities::from_json(&json!({})).encoding,
            Encoding::Utf16
        );
    }

    #[test]
    fn server_requests_get_answers() {
        let r = answer_server_request(
            "workspace/configuration",
            Some(&json!({"items": [{}, {}]})),
            json!(7),
        );
        assert_eq!(r["result"], json!([null, null]));
        let r = answer_server_request("x/unknown", None, json!("a"));
        assert_eq!(r["error"]["code"], -32601);
        assert_eq!(r["id"], "a");
    }
}
