//! SSH language-server setup and remote-document lifecycle.
use super::*;

pub(super) fn settings(config: &mtty_config::LspConfig) -> mtty_lsp::Settings {
    let server = |config: &mtty_config::LspServerConfig| mtty_lsp::ServerSettings {
        command: config.command.clone(),
        root_markers: config.root_markers.clone(),
        disabled: config.disabled,
    };
    mtty_lsp::Settings {
        disabled: config.disabled,
        servers: config
            .servers
            .iter()
            .map(|(key, value)| (key.clone(), server(value)))
            .collect(),
        remotes: config
            .remotes
            .iter()
            .filter(|(_, value)| !value.disabled)
            .map(|(target, value)| {
                (
                    target.clone(),
                    mtty_lsp::remote::Workspace {
                        root: value.root.clone().into(),
                        prefix: mtty_ui::ssh::language_server_prefix(target),
                        servers: value
                            .servers
                            .iter()
                            .map(|(key, value)| (key.clone(), server(value)))
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}
impl State {
    pub(super) fn resolve_remote_lsp_documents(&mut self) {
        let mut pending = Vec::new();
        for editor in self.tabs.iter_mut().flat_map(|tab| &mut tab.editors) {
            let Some(remote) = &editor.remote else {
                continue;
            };
            if editor.is_view_only()
                || editor.remote_original.is_none()
                || editor.remote_lsp.path.is_some()
                || editor.remote_lsp.pending
                || editor.remote_lsp.error.is_some()
                || editor.doc.rope().len_bytes() > mtty_lsp::MAX_DOCUMENT_BYTES
            {
                continue;
            }
            let Some(markers) =
                self.lsp
                    .remote_markers(&remote.dest, editor.language(), &editor.path)
            else {
                continue;
            };
            pending.push((
                editor.id.clone(),
                remote.dest.clone(),
                remote.path.clone(),
                markers,
            ));
            editor.remote_lsp.pending = true;
        }
        for (id, dest, path, markers) in pending {
            let epoch = self.lsp_generation;
            self.spawn_job(move || JobDone::RemoteLspResolved {
                epoch,
                id,
                dest: dest.clone(),
                path: path.clone(),
                result: mtty_ui::ssh::resolve_lsp_context(&dest, &path, &markers)
                    .map_err(|error| error.to_string()),
            });
        }
    }
    pub(super) fn restart_language_servers(&mut self) {
        self.lsp_generation = self.lsp_generation.saturating_add(1);
        let epoch = self.lsp_generation;
        self.spawn_job(move || {
            let (config, error) = mtty_config::Config::load_checked();
            JobDone::LspSettingsLoaded {
                epoch,
                result: match error {
                    Some(error) => Err(error),
                    None => Ok(config.lsp),
                },
            }
        });
    }
    pub(super) fn install_lsp_settings(&mut self, config: mtty_config::LspConfig) {
        let proxy = self.proxy.clone();
        let next = mtty_lsp::Lsp::new(
            settings(&config),
            Arc::new(move || {
                let _ = proxy.send_event(HostEvent::Wake);
            }),
        );
        let mut old = std::mem::replace(&mut self.lsp, next);
        old.shutdown();
        std::thread::spawn(move || drop(old));
        for editor in self.tabs.iter_mut().flat_map(|tab| &mut tab.editors) {
            editor.remote_lsp = Default::default();
            editor.diagnostics.clear();
        }
        self.hover = None;
        self.completion = None;
    }
    pub(super) fn open_remote_definition(
        &mut self,
        dest: String,
        path: String,
        range: mtty_lsp::LspRange,
        encoding: mtty_lsp::Encoding,
    ) {
        let existing = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.editors
                .iter()
                .find(|editor| {
                    editor.remote.as_ref().is_some_and(|remote| {
                        remote.dest == dest
                            && (remote.path == path
                                || editor.remote_lsp.path.as_deref() == Some(&path))
                    })
                })
                .map(|editor| (index, editor.id.clone()))
        });
        if let Some((index, id)) = existing {
            self.active_tab = index;
            self.tabs[index].active = id;
            self.position_lsp_target(range, encoding);
        } else {
            let epoch = self.lsp_generation;
            self.spawn_job(move || JobDone::RemoteDefinitionRead {
                epoch,
                result: mtty_ui::ssh::read_remote(&dest, &path),
                dest,
                path,
                range,
                encoding,
            });
        }
    }
    pub(super) fn position_lsp_target(
        &mut self,
        range: mtty_lsp::LspRange,
        encoding: mtty_lsp::Encoding,
    ) {
        if let Some(editor) = self.active_editor_mut() {
            let at = mtty_lsp::pos_to_char(editor.doc.rope(), range.start, encoding);
            let line = editor.doc.rope().char_to_line(at);
            editor.go_to_line(line);
            editor.doc.set_selection(mtty_editor::Selection::cursor(at));
            editor.reveal_cursor();
        }
        self.completion = None;
        self.hover = None;
    }
}
