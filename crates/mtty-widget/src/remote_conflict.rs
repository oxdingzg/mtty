//! Remote conflict review. Overwrite is checked against the revision shown.
use super::*;

pub(super) struct Conflict {
    pub id: Option<String>,
    pub dest: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

impl State {
    pub(super) fn read_remote_conflict(&self, id: Option<String>, dest: String, path: String) {
        self.spawn_job(move || JobDone::RemoteConflict {
            result: mtty_ui::ssh::read_remote(&dest, &path),
            id,
            dest,
            path,
        });
    }

    pub(super) fn remote_conflict_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(conflict) = self.remote_conflicts.first() else {
            return;
        };
        let local = match &conflict.id {
            Some(id) => self
                .tabs
                .iter()
                .flat_map(|tab| &tab.editors)
                .find(|ed| {
                    &ed.id == id
                        && ed.remote.as_ref().is_some_and(|remote| {
                            remote.dest == conflict.dest && remote.path == conflict.path
                        })
                })
                .map(|ed| (ed.doc.rope().to_string(), ed.doc.to_bytes(), ed.saving)),
            None => self
                .editor
                .as_ref()
                .filter(|ed| {
                    ed.remote.as_ref().is_some_and(|(dest, path)| {
                        dest == &conflict.dest && path == &conflict.path
                    })
                })
                .map(|ed| (ed.text.clone(), ed.text.as_bytes().to_vec(), ed.saving)),
        };
        let Some((local_text, local_bytes, saving)) = local else {
            self.remote_conflicts.remove(0);
            return;
        };
        let remote_text = mtty_editor::Document::from_bytes(&conflict.bytes)
            .map(|doc| doc.rope().to_string())
            .unwrap_or_else(|_| String::from_utf8_lossy(&conflict.bytes).into_owned());
        let lang = self.lang;
        let mut open = true;
        let mut action = 0;
        app_window(t(lang, "Remote Save Conflict", "远程保存冲突"), ctx)
            .open(&mut open)
            .default_size([850.0, 550.0])
            .show(ctx, |ui| window_body(ui, |ui| {
                ui.label(format!("{}:{}", conflict.dest, conflict.path));
                ui.label(t(lang, "The remote file changed. Your edits are preserved. Saving checks the remote revision shown below again.", "远程文件已变化，本地编辑已保留。保存时会再次检查下方展示的远程版本。"));
                ui.columns(2, |columns| {
                    for (index, (label, text)) in [(t(lang, "Your edits", "本地编辑"), &local_text), (t(lang, "Remote revision", "远程版本"), &remote_text)].into_iter().enumerate() {
                        columns[index].label(egui::RichText::new(label).strong());
                        egui::ScrollArea::both().id_salt(("remote-conflict", index)).max_height(380.0).show(&mut columns[index], |ui| {
                            ui.label(egui::RichText::new(text).monospace());
                        });
                    }
                });
                ui.add_enabled_ui(!saving, |ui| ui.horizontal_wrapped(|ui| {
                    if ui.button(t(lang, "Keep local edits", "保留本地编辑")).clicked() { action = 1; }
                    if ui.button(t(lang, "Reload remote (discard local edits)", "重新加载远程（丢弃本地编辑）")).clicked() { action = 2; }
                    if ui.button(t(lang, "Save local over reviewed revision", "用本地内容替换已审阅版本")).clicked() { action = 3; }
                }));
            }));
        if action == 0 && open {
            return;
        }
        let conflict = self.remote_conflicts.remove(0);
        if action == 2 {
            if let Some(id) = &conflict.id {
                if let Some(ed) = self
                    .tabs
                    .iter_mut()
                    .flat_map(|tab| &mut tab.editors)
                    .find(|ed| &ed.id == id)
                {
                    ed.remote_loaded(&conflict.bytes);
                    ed.remote_disk = None;
                }
            } else if let Some(ed) = &mut self.editor {
                ed.text = remote_text.clone();
                ed.mark_saved(remote_text);
            }
        } else if action == 3 {
            if let Some(id) = &conflict.id {
                if let Some(ed) = self
                    .tabs
                    .iter_mut()
                    .flat_map(|tab| &mut tab.editors)
                    .find(|ed| &ed.id == id)
                {
                    ed.saving = true;
                }
            } else if let Some(ed) = &mut self.editor {
                ed.saving = true;
            }
            self.spawn_job(move || JobDone::RemoteWrite {
                result: mtty_ui::ssh::write_remote_checked(
                    &conflict.dest,
                    &conflict.path,
                    &conflict.bytes,
                    &local_bytes,
                ),
                id: conflict.id,
                dest: conflict.dest,
                path: conflict.path,
                text: local_bytes,
            });
        }
        self.window.request_redraw();
    }
}
