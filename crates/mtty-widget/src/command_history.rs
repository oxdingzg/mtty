//! Bounded command output browsing, context sharing and reviewed reruns.
use super::*;

pub(super) struct View {
    pane: String,
    selected: Option<u64>,
    failures: bool,
    query: String,
    export: String,
    rerun: Option<String>,
    output: Option<Arc<mtty_core::CommandOutput>>,
    lines: Vec<String>,
}

impl View {
    pub(super) fn new(pane: String) -> Self {
        Self {
            pane,
            selected: None,
            failures: false,
            query: String::new(),
            export: std::env::temp_dir()
                .join("mtty-command-output.txt")
                .to_string_lossy()
                .into_owned(),
            rerun: None,
            output: None,
            lines: Vec::new(),
        }
    }
}

impl State {
    pub(super) fn command_history_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(mut view) = self.command_history.take() else {
            return;
        };
        let Some((tab, pane)) = self.tabs.iter().find_map(|tab| {
            tab.panes
                .iter()
                .find(|pane| pane.id == view.pane)
                .map(|pane| (tab, pane))
        }) else {
            return;
        };
        let host = tab
            .ssh_target
            .as_deref()
            .unwrap_or(t(self.lang, "Local", "本地"))
            .to_owned();
        let summaries: Vec<_> = pane
            .term
            .command_history()
            .iter()
            .rev()
            .map(|out| {
                (
                    out.id,
                    out.command
                        .clone()
                        .unwrap_or_else(|| format!("#{}", out.id)),
                    out.exit,
                    out.elapsed_ms,
                )
            })
            .collect();
        if !summaries.iter().any(|row| Some(row.0) == view.selected) {
            view.selected = summaries.first().map(|row| row.0);
            view.rerun = None;
        }
        if view.output.as_ref().map(|out| out.id) != view.selected {
            view.output = pane
                .term
                .command_history()
                .iter()
                .find(|out| Some(out.id) == view.selected)
                .cloned()
                .map(Arc::new);
            view.lines = view
                .output
                .as_ref()
                .map(|out| {
                    out.text
                        .lines()
                        .map(|line| {
                            let mut displayed: String = line.chars().take(4096).collect();
                            if line.chars().nth(4096).is_some() {
                                displayed.push_str(" …");
                            }
                            displayed
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        let output = view.output.clone();
        let ready = !self.read_only && !pane.term.command_running();
        let cwd_matches = output.as_ref().is_some_and(|out| {
            out.cwd
                .as_deref()
                .is_some_and(|cwd| Some(cwd) == pane.term.cwd())
        });
        let bracketed = pane.term.screen().bracketed_paste();
        let lang = self.lang;
        let mut open = true;
        let mut action = 0;
        app_window(t(lang, "Command Output History", "命令输出历史"), ctx)
            .open(&mut open).default_size([850.0, 600.0])
            .show(ctx, |ui| window_body(ui, |ui| {
                ui.label(format!("{host} · {}", view.pane));
                ui.label(t(lang, "Up to 50 commands / 8 MiB per pane; history is kept for this session only.", "每个窗格最多保留 50 条命令 / 8 MiB 输出，仅保留当前会话历史。"));
                ui.horizontal(|ui| {
                    ui.checkbox(&mut view.failures, t(lang, "Failures only", "仅失败命令"));
                    ui.add(egui::TextEdit::singleline(&mut view.query).hint_text(t(lang, "Filter commands…", "筛选命令…")));
                });
                egui::ScrollArea::vertical().id_salt("command-list").max_height(160.0).show(ui, |ui| {
                    for (id, command, exit, elapsed) in &summaries {
                        if view.failures && !exit.is_some_and(|code| code != 0) { continue; }
                        if !command.to_lowercase().contains(&view.query.to_lowercase()) { continue; }
                        let code = exit.map_or("?".to_owned(), |code| code.to_string());
                        if ui.selectable_label(view.selected == Some(*id), format!("#{id} · exit {code} · {elapsed} ms · {command}")).clicked() { view.selected = Some(*id); view.rerun = None; }
                    }
                });
                ui.separator();
                let Some(out) = &output else { ui.label(t(lang, "No captured output (requires shell integration).", "尚无捕获的输出（需要 shell 集成）。")); return; };
                ui.label(format!("cwd: {}", out.cwd.as_deref().unwrap_or("?")));
                if out.truncated { ui.label(t(lang, "Output truncated at 2 MiB", "输出已截断至 2 MiB")); }
                ui.horizontal_wrapped(|ui| {
                    if ui.button(t(lang, "Copy output", "复制输出")).clicked() { ctx.copy_text(out.text.clone()); }
                    if ui.button(t(lang, "To Composer", "发到 Composer")).clicked() { action = 1; }
                    if ui.add_enabled(!self.read_only, egui::Button::new(t(lang, "Send to Agent", "发送给 Agent"))).clicked() { action = 2; }
                    if ui.add_enabled(out.command.is_some(), egui::Button::new(t(lang, "Review rerun…", "审阅后重跑…"))).clicked() { view.rerun = out.command.clone(); }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut view.export).hint_text(t(lang, "Export path", "导出路径")));
                    if ui.button(t(lang, "Export output", "导出输出")).clicked() { action = 3; }
                });
                if let Some(command) = &mut view.rerun {
                    ui.separator();
                    ui.label(t(lang, "Review the command before executing it in its original pane and directory.", "执行前请审阅命令；将在原窗格与原目录执行。"));
                    ui.add(egui::TextEdit::multiline(command).font(egui::TextStyle::Monospace));
                    if !ready || !cwd_matches { ui.label(t(lang, "Return the original pane to its shell prompt and original directory first.", "请先让原窗格回到 shell 提示符与原目录。")); }
                    let multiline = command.contains(['\n', '\r']);
                    if ui.add_enabled(ready && cwd_matches && (!multiline || bracketed) && !command.trim().is_empty(), egui::Button::new(t(lang, "Execute reviewed command", "执行已审阅命令"))).clicked() { action = 4; }
                }
                let height = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::both().id_salt("command-output").max_height(250.0).show_rows(ui, height, view.lines.len(), |ui, rows| {
                    for row in rows { ui.add(egui::Label::new(egui::RichText::new(&view.lines[row]).monospace()).wrap_mode(egui::TextWrapMode::Extend)); }
                });
            }));
        if let Some(out) = output {
            // Indented context cannot be closed by fences in command output.
            let indented = if matches!(action, 1 | 2) {
                out.text
                    .lines()
                    .map(|line| format!("    {line}\n"))
                    .collect::<String>()
            } else {
                String::new()
            };
            let context = format!(
                "host: {host}\ncwd: {}\ncommand: {}\nexit: {:?} · {} ms{}\n\n{indented}",
                out.cwd.as_deref().unwrap_or("?"),
                out.command.as_deref().unwrap_or("?"),
                out.exit,
                out.elapsed_ms,
                if out.truncated { " · truncated" } else { "" }
            );
            match action {
                1 => {
                    let draft = self.composer.take().unwrap_or_default();
                    self.composer = Some(format!("{draft}\n\n{context}"));
                }
                2 => {
                    self.focus_attention_pane(&view.pane);
                    self.send_to_agent(&context);
                }
                3 => {
                    let path = view.export.clone();
                    self.spawn_job(move || JobDone::CommandExport {
                        result: std::fs::write(&path, &out.text),
                        path,
                    });
                }
                4 => {
                    if let Some(command) = view.rerun.take() {
                        self.focus_attention_pane(&view.pane);
                        self.paste(&command);
                        self.write_input(b"\r");
                        open = false;
                    }
                }
                _ => {}
            }
        }
        if open {
            self.command_history = Some(view);
        }
    }
}
