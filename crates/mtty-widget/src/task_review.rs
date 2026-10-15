//! Task review, feedback routing and verification captured by a dedicated pane.
use super::*;
use mtty_ui::task_review::{self as model, Comment, Review, Verification};
use mtty_ui::tasks::Task;

pub(super) struct View {
    pub task: Task,
    pub data: Option<Review>,
    pub loading: bool,
    selected: Option<String>,
    cache: Option<(String, u64)>,
    lines: Vec<(Option<usize>, String)>,
    first_line: usize,
    last_line: usize,
    comment: String,
    check: String,
}

pub(super) struct PendingCheck {
    pane: String,
    task: Task,
    command: String,
    revision: u64,
}

enum Action {
    Refresh,
    Mark(String, u64, bool),
    Open(std::path::PathBuf, usize),
    Compose(Comment),
    Send(Comment),
    Check(String, u64),
}

impl State {
    pub(super) fn open_task_review(&mut self, task: Task) {
        self.task_review = Some(View {
            task: task.clone(),
            data: None,
            loading: true,
            selected: None,
            cache: None,
            lines: Vec::new(),
            first_line: 1,
            last_line: 1,
            comment: String::new(),
            check: "git diff --check".into(),
        });
        self.spawn_job(move || JobDone::TaskReviewLoaded {
            result: model::load(&task),
            task,
        });
    }

    pub(super) fn task_review_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(mut view) = self.task_review.take() else {
            return;
        };
        let lang = self.lang;
        let readonly = self.read_only;
        if let Some(data) = &view.data {
            if !data
                .snapshot
                .files
                .iter()
                .any(|file| Some(&file.path) == view.selected.as_ref())
            {
                view.selected = data.snapshot.files.first().map(|file| file.path.clone());
                view.comment.clear();
            }
            if let Some(file) = data
                .snapshot
                .files
                .iter()
                .find(|file| Some(&file.path) == view.selected.as_ref())
            {
                let key = (file.path.clone(), file.fingerprint);
                if view.cache.as_ref() != Some(&key) {
                    let numbers = model::line_numbers(&file.patch);
                    view.lines = file
                        .patch
                        .lines()
                        .zip(numbers)
                        .map(|(line, number)| {
                            let mut text: String = line.chars().take(4096).collect();
                            if line.chars().nth(4096).is_some() {
                                text.push_str(" …");
                            }
                            (number, text)
                        })
                        .collect();
                    view.cache = Some(key);
                }
            }
        }
        let pending = self
            .task_checks
            .iter()
            .any(|check| check.task.path == view.task.path);
        let mut open = true;
        let mut action = None;
        app_window(t(lang, "Task Review", "任务审阅"), ctx).open(&mut open)
            .default_size([1000.0, 650.0]).show(ctx, |ui| window_body(ui, |ui| {
                ui.label(egui::RichText::new(format!("{} · {}", view.task.name, view.task.branch)).strong());
                ui.label(view.task.path.display().to_string());
                ui.horizontal(|ui| {
                    if ui.add_enabled(!view.loading, egui::Button::new(t(lang, "Refresh changes", "刷新改动"))).clicked() { action = Some(Action::Refresh); }
                    if view.loading { ui.spinner(); }
                });
                let Some(data) = &view.data else { ui.label(t(lang, "Loading task changes…", "正在读取任务改动…")); return; };
                let reviewed = data.snapshot.files.iter().filter(|file| data.state.reviewed.get(&file.path) == Some(&file.fingerprint)).count();
                ui.label(format!("{} / {} {}", reviewed, data.snapshot.files.len(), t(lang, "files reviewed", "个文件已审阅")));
                if data.snapshot.partial { ui.colored_label(chrome_rgb(self.theme.chrome().warning), t(lang, "Partial or moving diff: refresh and open files to inspect remaining content.", "改动不完整或正在变化：请刷新并打开文件查看其余内容。")); }
                egui::ScrollArea::vertical().id_salt("review-files").max_height(125.0).show(ui, |ui| {
                    for file in &data.snapshot.files {
                        ui.horizontal(|ui| {
                            let mut checked = data.state.reviewed.get(&file.path) == Some(&file.fingerprint);
                            if ui.add_enabled(!view.loading && !file.truncated && !file.unstable, egui::Checkbox::without_text(&mut checked)).changed() { action = Some(Action::Mark(file.path.clone(), file.fingerprint, checked)); }
                            if ui.selectable_label(view.selected.as_ref() == Some(&file.path), &file.path).clicked() {
                                view.selected = Some(file.path.clone()); view.first_line = 1; view.last_line = 1; view.comment.clear();
                            }
                        });
                    }
                });
                ui.separator();
                if let Some(file) = data.snapshot.files.iter().find(|file| Some(&file.path) == view.selected.as_ref()) {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&file.path).strong());
                        if ui.button(t(lang, "Open file", "打开文件")).clicked() { action = Some(Action::Open(view.task.path.join(&file.path), view.first_line)); }
                    });
                    let height = ui.text_style_height(&egui::TextStyle::Monospace);
                    egui::ScrollArea::both().id_salt("review-patch").max_height(220.0).show_rows(ui, height, view.lines.len(), |ui, rows| {
                        for row in rows {
                            let (number, text) = &view.lines[row];
                            let label = format!("{:>5} {text}", number.map_or(String::new(), |line| line.to_string()));
                            let color = if text.starts_with('+') && !text.starts_with("+++") { self.theme.palette[2] }
                                else if text.starts_with('-') && !text.starts_with("---") { self.theme.palette[1] }
                                else if text.starts_with("@@") { self.theme.palette[6] }
                                else { self.theme.fg };
                            if ui.add(egui::Label::new(egui::RichText::new(label).monospace().color(chrome_rgb(color))).sense(egui::Sense::click()).wrap_mode(egui::TextWrapMode::Extend)).clicked() {
                                if let Some(line) = number { view.first_line = *line; view.last_line = *line; }
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(t(lang, "Comment lines", "评论行范围"));
                        ui.add(egui::DragValue::new(&mut view.first_line).range(1..=usize::MAX));
                        ui.label("–");
                        ui.add(egui::DragValue::new(&mut view.last_line).range(view.first_line..=usize::MAX));
                    });
                    ui.add(egui::TextEdit::multiline(&mut view.comment).desired_rows(2).hint_text(t(lang, "Feedback for this file and line range…", "对这个文件及行范围的反馈…")));
                    ui.horizontal(|ui| {
                        let feedback = || Comment { path: file.path.clone(), line_start: view.first_line, line_end: view.last_line.max(view.first_line), text: view.comment.clone(), patch: model::comment_context(&file.patch, view.first_line, view.last_line.max(view.first_line)), revision: data.snapshot.fingerprint };
                        if ui.add_enabled(!view.comment.trim().is_empty(), egui::Button::new(t(lang, "To Composer", "发到 Composer"))).clicked() { action = Some(Action::Compose(feedback())); }
                        if ui.add_enabled(!readonly && !view.comment.trim().is_empty(), egui::Button::new(t(lang, "Send feedback to task Agent", "发送反馈给任务 Agent"))).clicked() { action = Some(Action::Send(feedback())); }
                    });
                } else { ui.label(t(lang, "No changes in this task.", "此任务暂无改动。")); }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut view.check).hint_text(t(lang, "Verification command", "验证命令")));
                    if ui.add_enabled(!readonly && !pending && !view.check.trim().is_empty(), egui::Button::new(t(lang, "Run verification", "运行验证"))).clicked() { action = Some(Action::Check(view.check.clone(), data.snapshot.fingerprint)); }
                    if pending { ui.label(t(lang, "Running in its own terminal…", "正在独立终端中运行…")); }
                });
                ui.label(t(lang, "Task completion is separate from verification. Results refer to the revision that was checked.", "任务完成与验证通过分别展示。验证结果关联运行时的改动版本。"));
                for check in data.state.checks.iter().rev().take(5) {
                    let result = if check.exit==Some(0)&&!check.passed(){t(lang,"Evidence failed","证据校验失败")}else{match check.exit { Some(0) => t(lang, "Passed", "通过"), Some(_) => t(lang, "Failed", "失败"), None => t(lang, "No captured exit / cancelled", "未捕获退出码／已取消") }};
                    let stale = if check.revision != data.snapshot.fingerprint { t(lang, " · older revision", " · 较早版本") } else { "" };
                    let exit = check.exit.map_or_else(|| "?".to_owned(), |code| code.to_string());
                    ui.collapsing(format!("{result}{stale} · {} · exit {exit}", check.command), |ui| {
                        egui::ScrollArea::both().id_salt(("check-output", check.finished_at)).max_height(100.0).show(ui, |ui| { ui.label(egui::RichText::new(&check.output).monospace()); });
                    });
                }
                if !data.state.comments.is_empty() { ui.label(format!("{} {}", data.state.comments.len(), t(lang, "feedback messages recorded", "条反馈已记录"))); }
            }));
        let task = view.task.clone();
        match action {
            Some(Action::Refresh) => {
                view.loading = true;
                self.spawn_job(move || JobDone::TaskReviewLoaded {
                    result: model::load(&task),
                    task,
                });
            }
            Some(Action::Mark(path, fingerprint, checked)) => {
                view.loading = true;
                self.spawn_job(move || JobDone::TaskReviewLoaded {
                    result: model::mark_reviewed(&task, &path, fingerprint, checked),
                    task,
                });
            }
            Some(Action::Open(path, line)) => {
                if self.open_editor_ro(path, false) {
                    self.go_active_editor_to(Some(line), None);
                }
            }
            Some(Action::Compose(comment)) => {
                let draft = self.composer.take().unwrap_or_default();
                self.composer = Some(format!("{draft}\n\n{}", comment.prompt(&task)));
            }
            Some(Action::Send(comment)) => {
                if self.send_task_feedback(&task, &comment.prompt(&task)) {
                    view.comment.clear();
                    view.loading = true;
                    self.spawn_job(move || JobDone::TaskReviewLoaded {
                        result: model::record_comment(&task, comment),
                        task,
                    });
                }
            }
            Some(Action::Check(command, revision)) => {
                self.start_task_check(task, command, revision)
            }
            None => {}
        }
        if open {
            self.task_review = Some(view);
        }
    }

    fn send_task_feedback(&mut self, task: &Task, text: &str) -> bool {
        let root = std::fs::canonicalize(&task.path).unwrap_or_else(|_| task.path.clone());
        let candidate = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            tab.panes.iter().find_map(|pane| {
                let cwd = pane.term.cwd().map(std::path::Path::new)?;
                let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
                if !cwd.starts_with(&root) || self.mtp.agent_for(&pane.id).is_none() {
                    return None;
                }
                // A stale badge on a shell is not a live agent input target.
                let state = self.mtp.agent_for(&pane.id).and_then(|value| {
                    value
                        .get("state")
                        .and_then(|state| state.as_str())
                        .map(str::to_owned)
                });
                if pane.term.foreground_command().is_none()
                    && !matches!(
                        state.as_deref(),
                        Some("processing" | "awaiting" | "waiting")
                    )
                {
                    return None;
                }
                Some((index, pane.id.clone()))
            })
        });
        let Some((index, id)) = candidate else {
            self.show_notice(
                mtty_ui::i18n::t(
                    self.lang,
                    "No live Agent in this task worktree. Open/resume it, or use Composer.",
                    "此任务工作树中没有正在运行的 Agent。请打开／恢复 Agent，或使用 Composer。",
                )
                .into(),
            );
            return false;
        };
        self.active_tab = index;
        self.tabs[index].active = id.clone();
        if let Some(pane) = self.tabs[index].panes.iter_mut().find(|pane| pane.id == id) {
            let payload = input::encode_paste(text, pane.term.screen().bracketed_paste());
            pane.term.write(&payload);
            pane.term.write(b"\r");
            return true;
        }
        false
    }

    pub(super) fn start_task_check(&mut self, task: Task, command: String, revision: u64) {
        let before = self.tabs.len();
        self.new_tab_in(Some(task.path.clone()));
        if self.tabs.len() != before + 1 {
            return;
        }
        if let Some(tab) = self.tabs.last_mut() {
            tab.title = format!("check: {}", task.name);
            tab.title_set = true;
            if let Some(pane) = tab.panes.iter_mut().find(|pane| pane.id == tab.active) {
                pane.term.write(format!("{command}\r").as_bytes());
                self.task_checks.push(PendingCheck {
                    pane: pane.id.clone(),
                    task,
                    command,
                    revision,
                });
            }
        }
        self.publish_panes();
    }

    pub(super) fn poll_task_checks(&mut self) {
        let mut ready = Vec::new();
        self.task_checks.retain(|check| {
            let pane = self
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|pane| pane.id == check.pane);
            match pane {
                Some(pane) => match pane.term.last_command_output() {
                    Some(output) => {
                        ready.push((
                            check.task.clone(),
                            check.command.clone(),
                            check.revision,
                            output.exit,
                            output.text.clone(),
                        ));
                        false
                    }
                    None => true,
                },
                None => {
                    ready.push((
                        check.task.clone(),
                        check.command.clone(),
                        check.revision,
                        None,
                        "Terminal closed before an exit status was captured.".into(),
                    ));
                    false
                }
            }
        });
        for (task, command, revision, exit, output) in ready {
            self.spawn_job(move || {
                let check = Verification {
                    command,
                    revision,
                    exit,
                    output,
                    finished_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |duration| duration.as_secs()),
                    browser: None,
                };
                JobDone::TaskReviewLoaded {
                    result: model::record_check(&task, check),
                    task,
                }
            });
        }
    }
}
