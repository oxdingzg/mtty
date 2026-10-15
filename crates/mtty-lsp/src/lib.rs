//! UI-independent language-server lifecycle and document routing.
pub mod client;
pub mod env;
pub mod position;
pub mod protocol;
pub mod remote;
#[cfg(all(test, unix))]
mod remote_tests;
pub mod rpc;
pub mod servers;

pub use client::{Capabilities, Client, Incoming, Waker};
use mtty_editor::Rope;
pub use position::{char_to_pos, pos_to_char, Encoding, LspRange, Pos};
pub use protocol::{CompletionItem, Diagnostic};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
pub const MAX_DOCUMENT_BYTES: usize = 2 << 20;

#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub disabled: bool,
    pub servers: HashMap<String, ServerSettings>,
    pub remotes: HashMap<String, remote::Workspace>,
}
#[derive(Clone, Debug, Default)]
pub struct ServerSettings {
    pub command: Option<Vec<String>>,
    pub root_markers: Option<Vec<String>>,
    pub disabled: bool,
}
#[derive(Debug)]
pub enum Event {
    Diagnostics(PathBuf),
    Hover {
        path: PathBuf,
        at: usize,
        markdown: String,
    },
    Completion {
        path: PathBuf,
        at: usize,
        ticket: u64,
        items: Vec<CompletionItem>,
        incomplete: bool,
        encoding: Encoding,
    },
    Definition {
        source: PathBuf,
        at: usize,
        revision: u64,
        targets: Vec<(PathBuf, LspRange)>,
        encoding: Encoding,
    },
    Failed {
        server: String,
        message: String,
        configured: bool,
        paths: Vec<PathBuf>,
    },
}
struct Running {
    key: String,
    root: PathBuf,
    target: Option<String>,
    client: Client,
    failed: bool,
    configured: bool,
}
struct OpenDoc {
    server: usize,
    version: i32,
    revision: u64,
    uri: String,
    physical: String,
}
enum Pending {
    Hover {
        path: PathBuf,
        at: usize,
    },
    Completion {
        path: PathBuf,
        at: usize,
        ticket: u64,
    },
    Definition {
        path: PathBuf,
        at: usize,
        revision: u64,
    },
}
impl Pending {
    fn path(&self) -> &Path {
        match self {
            Self::Hover { path, .. }
            | Self::Completion { path, .. }
            | Self::Definition { path, .. } => path,
        }
    }
}

pub struct Lsp {
    settings: Settings,
    servers: Vec<Running>,
    docs: HashMap<PathBuf, OpenDoc>,
    diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    pending: HashMap<(usize, i64), Pending>,
    skipped: HashSet<PathBuf>,
    waker: Waker,
    next_ticket: u64,
    events: Vec<Event>,
}
impl Lsp {
    pub fn new(settings: Settings, waker: Waker) -> Self {
        Self {
            settings,
            servers: Vec::new(),
            docs: HashMap::new(),
            diagnostics: HashMap::new(),
            pending: HashMap::new(),
            skipped: HashSet::new(),
            waker,
            next_ticket: 0,
            events: Vec::new(),
        }
    }
    pub fn remote_configured(&self, target: &str) -> bool {
        !self.settings.disabled && self.settings.remotes.contains_key(target)
    }
    pub fn remote_markers(
        &self,
        target: &str,
        language: Option<&str>,
        path: &Path,
    ) -> Option<Vec<String>> {
        if !self.remote_configured(target) {
            return None;
        }
        let (key, _) = language.and_then(|language| servers::language(language, path))?;
        let remote = self.settings.remotes.get(target)?;
        let custom = remote.servers.get(key).cloned().unwrap_or_default();
        if custom.disabled {
            return None;
        }
        Some(custom.root_markers.unwrap_or_else(|| {
            servers::server(key)
                .unwrap()
                .root_markers
                .iter()
                .map(|marker| marker.to_string())
                .collect()
        }))
    }
    pub fn sync(&mut self, path: &Path, language: Option<&str>, rope: &Rope, revision: u64) {
        self.sync_document(path, path, language, rope, revision, None);
    }
    pub fn sync_remote(
        &mut self,
        key: &Path,
        path: &Path,
        root: &Path,
        language: Option<&str>,
        rope: &Rope,
        revision: u64,
    ) {
        let Some((target, _)) = remote::split_key(key) else {
            return;
        };
        let Some(mut workspace) = self.settings.remotes.get(&target).cloned() else {
            return;
        };
        if workspace.root.as_os_str().is_empty() {
            workspace.root = root.to_path_buf();
        }
        if !workspace.root.to_string_lossy().starts_with('/') || workspace.prefix.is_empty() {
            if self.skipped.insert(key.to_path_buf()) {
                self.events.push(Event::Failed {
                    server: target,
                    message:
                        "remote LSP needs an absolute POSIX workspace root and an SSH transport"
                            .into(),
                    configured: true,
                    paths: vec![key.to_path_buf()],
                });
            }
            return;
        }
        self.sync_document(
            key,
            path,
            language,
            rope,
            revision,
            Some((target, workspace)),
        );
    }
    fn sync_document(
        &mut self,
        key: &Path,
        path: &Path,
        language: Option<&str>,
        rope: &Rope,
        revision: u64,
        remote_context: Option<(String, remote::Workspace)>,
    ) {
        if self.settings.disabled || self.skipped.contains(key) {
            return;
        }
        if rope.len_bytes() > MAX_DOCUMENT_BYTES {
            if let Some(doc) = self.docs.remove(key) {
                self.servers[doc.server].client.notify(
                    "textDocument/didClose",
                    json!({"textDocument":{"uri":doc.uri}}),
                );
            }
            if self.diagnostics.remove(key).is_some() {
                self.events.push(Event::Diagnostics(key.to_path_buf()));
            }
            return;
        }
        if let Some(doc) = self.docs.get_mut(key) {
            if doc.revision == revision {
                return;
            }
            doc.revision = revision;
            doc.version += 1;
            let running = &mut self.servers[doc.server];
            if running.failed {
                return;
            }
            running.client.notify("textDocument/didChange",json!({"textDocument":{"uri":doc.uri,"version":doc.version},"contentChanges":[{"text":rope.to_string()}]}));
            return;
        }
        let Some((language_key, language_id)) =
            language.and_then(|language| servers::language(language, path))
        else {
            self.skipped.insert(key.to_path_buf());
            return;
        };
        let Some(spec) = servers::server(language_key) else {
            return;
        };
        let custom = match &remote_context {
            Some((_, workspace)) => workspace.servers.get(language_key),
            None => self.settings.servers.get(language_key),
        }
        .cloned()
        .unwrap_or_default();
        if custom.disabled {
            self.skipped.insert(key.to_path_buf());
            return;
        }
        let markers = custom.root_markers.clone().unwrap_or_else(|| {
            spec.root_markers
                .iter()
                .map(|marker| marker.to_string())
                .collect()
        });
        let root = match &remote_context {
            Some((_, workspace)) => workspace.root.clone(),
            None => servers::find_root(path, &markers),
        };
        let target = remote_context.as_ref().map(|(target, _)| target.clone());
        let index = match self.servers.iter().position(|server| {
            server.key == language_key && server.root == root && server.target == target
        }) {
            Some(index) => index,
            None => {
                let command = custom
                    .command
                    .clone()
                    .unwrap_or_else(|| spec.command.iter().map(|part| part.to_string()).collect());
                let client = match &remote_context {
                    Some((_, workspace)) => Client::start_remote(
                        remote::command(workspace, &command),
                        &root,
                        self.waker.clone(),
                    ),
                    None => Client::start(command, &root, self.waker.clone()),
                };
                self.servers.push(Running {
                    key: language_key.into(),
                    root,
                    target: target.clone(),
                    client,
                    failed: false,
                    configured: remote_context.is_some() || custom.command.is_some(),
                });
                self.servers.len() - 1
            }
        };
        if self.servers[index].failed {
            self.skipped.insert(key.to_path_buf());
            return;
        }
        let uri = if remote_context.is_some() {
            remote::file_uri(&path.to_string_lossy())
        } else {
            position::path_to_uri(path)
        };
        self.servers[index].client.notify("textDocument/didOpen",json!({"textDocument":{"uri":uri,"languageId":language_id,"version":1,"text":rope.to_string()}}));
        self.docs.insert(
            key.to_path_buf(),
            OpenDoc {
                server: index,
                version: 1,
                revision,
                uri,
                physical: path.to_string_lossy().into_owned(),
            },
        );
    }
    pub fn retain(&mut self, open: &HashSet<PathBuf>) {
        let gone: Vec<_> = self
            .docs
            .keys()
            .filter(|path| !open.contains(*path))
            .cloned()
            .collect();
        for path in gone {
            if let Some(doc) = self.docs.remove(&path) {
                if !self.servers[doc.server].failed {
                    self.servers[doc.server].client.notify(
                        "textDocument/didClose",
                        json!({"textDocument":{"uri":doc.uri}}),
                    );
                }
            }
            self.diagnostics.remove(&path);
        }
        self.skipped.retain(|path| open.contains(path));
        self.pending
            .retain(|_, pending| open.contains(pending.path()));
    }
    pub fn saved(&mut self, path: &Path) {
        if let Some(doc) = self.docs.get(path) {
            let server = &mut self.servers[doc.server];
            if !server.failed {
                server.client.notify(
                    "textDocument/didSave",
                    json!({"textDocument":{"uri":doc.uri}}),
                );
            }
        }
    }
    pub fn handles(&self, path: &Path) -> bool {
        self.docs
            .get(path)
            .is_some_and(|doc| !self.servers[doc.server].failed)
    }
    pub fn ready(&self, path: &Path) -> bool {
        self.handles(path)
            && self
                .docs
                .get(path)
                .and_then(|doc| self.servers[doc.server].client.capabilities())
                .is_some()
    }
    pub fn encoding(&self, path: &Path) -> Encoding {
        self.docs
            .get(path)
            .and_then(|doc| self.servers[doc.server].client.capabilities())
            .map(|caps| caps.encoding)
            .unwrap_or_default()
    }
    pub fn trigger_characters(&self, path: &Path) -> Vec<String> {
        self.docs
            .get(path)
            .and_then(|doc| self.servers[doc.server].client.capabilities())
            .map(|caps| caps.trigger_characters)
            .unwrap_or_default()
    }
    pub fn diagnostics(&self, path: &Path) -> &[Diagnostic] {
        self.diagnostics.get(path).map_or(&[], Vec::as_slice)
    }
    fn position_params(&self, path: &Path, rope: &Rope, at: usize) -> serde_json::Value {
        json!({"textDocument":{"uri":self.docs.get(path).map(|doc|doc.uri.clone()).unwrap_or_else(||position::path_to_uri(path))},"position":char_to_pos(rope,at,self.encoding(path)).to_json()})
    }
    fn send(
        &mut self,
        path: &Path,
        method: &str,
        params: serde_json::Value,
        pending: Pending,
    ) -> bool {
        let Some(index) = self.docs.get(path).map(|doc| doc.server) else {
            return false;
        };
        let running = &mut self.servers[index];
        if running.failed {
            return false;
        }
        let Some(caps) = running.client.capabilities() else {
            return false;
        };
        if !match method {
            "textDocument/hover" => caps.hover,
            "textDocument/completion" => caps.completion,
            "textDocument/definition" => caps.definition,
            _ => true,
        } {
            return false;
        }
        let id = running.client.request(method, params);
        self.pending.insert((index, id), pending);
        true
    }
    pub fn hover(&mut self, path: &Path, rope: &Rope, at: usize) -> bool {
        self.send(
            path,
            "textDocument/hover",
            self.position_params(path, rope, at),
            Pending::Hover {
                path: path.to_path_buf(),
                at,
            },
        )
    }
    pub fn completion(
        &mut self,
        path: &Path,
        rope: &Rope,
        at: usize,
        trigger: Option<&str>,
    ) -> Option<u64> {
        let mut params = self.position_params(path, rope, at);
        params["context"] = match trigger {
            Some(character) => json!({"triggerKind":2,"triggerCharacter":character}),
            None => json!({"triggerKind":1}),
        };
        self.next_ticket += 1;
        let ticket = self.next_ticket;
        self.send(
            path,
            "textDocument/completion",
            params,
            Pending::Completion {
                path: path.to_path_buf(),
                at,
                ticket,
            },
        )
        .then_some(ticket)
    }
    pub fn definition(&mut self, path: &Path, rope: &Rope, at: usize) -> bool {
        let revision = self.docs.get(path).map(|doc| doc.revision).unwrap_or(0);
        self.send(
            path,
            "textDocument/definition",
            self.position_params(path, rope, at),
            Pending::Definition {
                path: path.to_path_buf(),
                at,
                revision,
            },
        )
    }
    fn path_for_uri(&self, index: usize, uri: &str) -> Option<PathBuf> {
        let Some(target) = &self.servers[index].target else {
            return position::uri_to_path(uri);
        };
        let physical = remote::uri_path(uri)?;
        self.docs
            .iter()
            .find(|(_, doc)| doc.server == index && doc.physical == physical)
            .map(|(key, _)| key.clone())
            .or_else(|| Some(remote::document_key(target, &physical)))
    }
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = std::mem::take(&mut self.events);
        for index in 0..self.servers.len() {
            for item in self.servers[index].client.poll() {
                match item {
                    Incoming::Ready => {}
                    Incoming::Failed(message) => {
                        let server = &mut self.servers[index];
                        if server.failed {
                            continue;
                        }
                        server.failed = true;
                        let name = server
                            .target
                            .as_ref()
                            .map(|target| format!("{} @ {target}", server.key))
                            .unwrap_or_else(|| server.key.clone());
                        let configured = server.configured;
                        server.client.stop();
                        let paths: Vec<_> = self
                            .docs
                            .iter()
                            .filter(|(_, doc)| doc.server == index)
                            .map(|(path, _)| path.clone())
                            .collect();
                        events.push(Event::Failed {
                            server: name,
                            message,
                            configured,
                            paths: paths.clone(),
                        });
                        for path in paths {
                            if self.diagnostics.remove(&path).is_some() {
                                events.push(Event::Diagnostics(path));
                            }
                        }
                        self.pending.retain(|(server, _)| *server != index);
                    }
                    Incoming::Notification { method, params } => {
                        if method == "textDocument/publishDiagnostics" {
                            if let Some((path, list)) =
                                protocol::diagnostics_using(&params, |uri| {
                                    self.path_for_uri(index, uri)
                                })
                            {
                                self.diagnostics.insert(path.clone(), list);
                                events.push(Event::Diagnostics(path));
                            }
                        }
                    }
                    Incoming::Response { id, result } => {
                        let Some(pending) = self.pending.remove(&(index, id)) else {
                            continue;
                        };
                        let result = result.unwrap_or(serde_json::Value::Null);
                        let encoding = self.servers[index]
                            .client
                            .capabilities()
                            .map(|caps| caps.encoding)
                            .unwrap_or_default();
                        match pending {
                            Pending::Hover { path, at } => {
                                if let Some(markdown) = protocol::hover_text(&result) {
                                    events.push(Event::Hover { path, at, markdown });
                                }
                            }
                            Pending::Completion { path, at, ticket } => {
                                let (items, incomplete) = protocol::completion_items(&result);
                                events.push(Event::Completion {
                                    path,
                                    at,
                                    ticket,
                                    items,
                                    incomplete,
                                    encoding,
                                });
                            }
                            Pending::Definition { path, at, revision } => {
                                events.push(Event::Definition {
                                    source: path,
                                    at,
                                    revision,
                                    targets: protocol::locations_using(&result, |uri| {
                                        self.path_for_uri(index, uri)
                                    }),
                                    encoding,
                                })
                            }
                        }
                    }
                }
            }
        }
        events
    }
    pub fn shutdown(&mut self) {
        for server in &mut self.servers {
            server.client.shutdown();
        }
    }
}
